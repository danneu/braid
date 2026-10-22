//! Parser for a device-mapper UUID as sysfs shows it in
//! `/sys/block/dm-N/dm/uuid`.
//!
//! cryptsetup sets the dm UUID once, when it activates a mapping
//! (`reference/cryptsetup/lib/libdevmapper.c#dm_prepare_uuid`):
//! `CRYPT-<type>-<header UUID without dashes>-<mapper name>`. The kernel
//! refuses to change a dm UUID once set, so for a LUKS mapping it names the
//! container cryptsetup unlocked for as long as the mapping exists.

use crate::types::LuksUuid;

/// The dm UUID prefixes cryptsetup writes for a LUKS1 or LUKS2 mapping.
/// `CRYPT-LUKS2-OPAL-` and `CRYPT-SUBDEV-` deliberately do not match.
const LUKS_PREFIXES: [&str; 2] = ["CRYPT-LUKS1-", "CRYPT-LUKS2-"];

/// Hex digits in a dashless UUID.
const UUID_HEX_LEN: usize = 32;

/// What a dm UUID says about a mapping's LUKS identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmUuid {
    /// A cryptsetup LUKS1 or LUKS2 mapping of this container.
    Luks(LuksUuid),
    /// Any other dm UUID: none, plain dm-crypt, a cryptsetup subdevice, LVM,
    /// or text cryptsetup never writes. Carries the text without its trailing
    /// newline, for diagnostics.
    Other(String),
}

/// Classify the contents of a `dm/uuid` sysfs file. The name after the hex
/// digits is not compared with the mapper's current name: a rename keeps the
/// old suffix, and cryptsetup itself ignores it
/// (`reference/cryptsetup/lib/libdevmapper.c#lookup_dm_dev_by_uuid`).
pub fn parse_dm_uuid(content: &str) -> DmUuid {
    let text = content.strip_suffix('\n').unwrap_or(content);
    match luks_uuid_in(text) {
        Some(uuid) => DmUuid::Luks(uuid),
        None => DmUuid::Other(text.to_owned()),
    }
}

/// The LUKS UUID a `CRYPT-LUKS{1,2}-<32 hex>-<name>` dm UUID carries.
fn luks_uuid_in(text: &str) -> Option<LuksUuid> {
    let rest = LUKS_PREFIXES
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))?;
    let hex = rest.get(..UUID_HEX_LEN)?;
    let name = rest.get(UUID_HEX_LEN..)?.strip_prefix('-')?;
    if name.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // `LuksUuid::parse` re-dashes and lowercases, the same canonical form the
    // `cryptsetup luksUUID` parser produces.
    LuksUuid::parse(hex).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "4f2e8a1c9b3d4e5fa6b7c8d9e0f1a2b3";
    const DASHED: &str = "4f2e8a1c-9b3d-4e5f-a6b7-c8d9e0f1a2b3";

    fn luks(uuid: &str) -> DmUuid {
        DmUuid::Luks(LuksUuid::parse(uuid).unwrap())
    }

    fn other(text: &str) -> DmUuid {
        DmUuid::Other(text.to_owned())
    }

    #[test]
    fn luks2_dm_uuid_yields_dashed_luks_uuid() {
        assert_eq!(
            parse_dm_uuid(&format!("CRYPT-LUKS2-{HEX}-braid-disk1\n")),
            luks(DASHED)
        );
    }

    #[test]
    fn luks1_dm_uuid_yields_luks_uuid() {
        assert_eq!(
            parse_dm_uuid(&format!("CRYPT-LUKS1-{HEX}-braid-disk1\n")),
            luks(DASHED)
        );
    }

    #[test]
    fn uppercase_hex_canonicalizes_to_lowercase() {
        assert_eq!(
            parse_dm_uuid(&format!(
                "CRYPT-LUKS2-{}-braid-disk1\n",
                HEX.to_ascii_uppercase()
            )),
            luks(DASHED)
        );
    }

    #[test]
    fn renamed_mapper_keeps_identity_from_old_suffix() {
        assert_eq!(
            parse_dm_uuid(&format!("CRYPT-LUKS2-{HEX}-braid-disk1\n")),
            parse_dm_uuid(&format!("CRYPT-LUKS2-{HEX}-braid-WRONG\n"))
        );
    }

    #[test]
    fn missing_trailing_newline_is_accepted() {
        assert_eq!(
            parse_dm_uuid(&format!("CRYPT-LUKS2-{HEX}-braid-disk1")),
            luks(DASHED)
        );
    }

    // Intent: every dm UUID that is not a cryptsetup LUKS mapping of a
    //   well-formed UUID classifies as Other, never as a LUKS identity.
    // Why it exists: the dm UUID is the only evidence braid has for an open
    //   mapper's identity, so a malformed or foreign value must stay
    //   unverifiable rather than prove membership.
    // Scenario: empty files, plain dm-crypt and LVM devices, cryptsetup's own
    //   non-LUKS types, and truncated or corrupted LUKS UUIDs.
    #[test]
    fn non_luks_and_malformed_dm_uuids_are_other() {
        let short = &HEX[..31];
        let long = format!("{HEX}0");
        let non_hex = format!("{}g", &HEX[..31]);
        let cases = [
            String::new(),
            "\n".to_owned(),
            "CRYPT-PLAIN-braid-disk1\n".to_owned(),
            format!("CRYPT-SUBDEV-{HEX}-braid-disk1_dif\n"),
            format!("CRYPT-LUKS2-OPAL-{HEX}-braid-disk1\n"),
            format!("CRYPT-TEMP-{HEX}-temporary-cryptsetup-1\n"),
            "LVM-abcdefabcdefabcdefabcdefabcdefabcdef\n".to_owned(),
            format!("LUKS2-{HEX}-braid-disk1\n"),
            format!("crypt-luks2-{HEX}-braid-disk1\n"),
            format!("CRYPT-LUKS2-{short}-braid-disk1\n"),
            format!("CRYPT-LUKS2-{long}-braid-disk1\n"),
            format!("CRYPT-LUKS2-{non_hex}-braid-disk1\n"),
            format!("CRYPT-LUKS2-{DASHED}-braid-disk1\n"),
            format!("CRYPT-LUKS2-{HEX}braid-disk1\n"),
            format!("CRYPT-LUKS2-{HEX}-\n"),
            format!("CRYPT-LUKS2-{HEX}\n"),
            "CRYPT-LUKS2-\n".to_owned(),
        ];
        for case in cases {
            let text = case.strip_suffix('\n').unwrap_or(&case);
            assert_eq!(parse_dm_uuid(&case), other(text), "input {case:?}");
        }
    }
}
