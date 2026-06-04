use crate::cmd::RawCommandOutput;
use crate::types::LuksUuid;

use super::ParseError;
use super::types::CryptsetupLuksUuidOutput;

pub fn cryptsetup_luks_uuid_reports_not_luks(raw: &RawCommandOutput) -> bool {
    raw.exit_status != 0
        && raw
            .stderr
            .to_ascii_lowercase()
            .contains("not a valid luks device")
}

pub fn parse_cryptsetup_luks_uuid(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksUuidOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let trimmed = raw.stdout.trim();

    // Route through LuksUuid::parse so canonicalization (lowercase
    // hyphenated form) is the single source of truth. LuksUuid::parse
    // owns validation; the previous separate uuid::Uuid::parse_str
    // sanity check is redundant and is removed here.
    let uuid = LuksUuid::parse(trimmed).map_err(|e| ParseError::InvalidText {
        cmd: raw.cmd.clone(),
        detail: format!("not a valid UUID: {trimmed:?} -- {}", e.detail),
    })?;

    Ok(CryptsetupLuksUuidOutput { uuid })
}

/// Extract the `UUID:` field from a `cryptsetup luksDump` text body and
/// route it through `LuksUuid::parse`. Co-located with
/// `parse_cryptsetup_luks_uuid` so both parsers share the same
/// canonicalization contract and the value-type-to-source relationship
/// stays one-hop (one file per producing command would split the
/// `LuksUuid` value-type from the producer parser).
pub fn parse_cryptsetup_luks_uuid_from_dump(
    raw: &RawCommandOutput,
) -> Result<LuksUuid, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let raw_value = raw.stdout.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("UUID:")
            .map(|rest| rest.trim().to_owned())
    });

    let raw_value = raw_value.ok_or_else(|| ParseError::MissingField {
        cmd: raw.cmd.clone(),
        field: "UUID".into(),
    })?;

    LuksUuid::parse(&raw_value).map_err(|e| ParseError::InvalidValue {
        cmd: raw.cmd.clone(),
        field: "UUID".into(),
        raw: e.raw,
        detail: e.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-26.05/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-26.05 fixtures) ---

    #[test]
    fn luks_uuid_parses_nixos_26_05() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: fixture("cryptsetup-luks-uuid.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_uuid(&raw).unwrap();
        // UUID is random per VM run -- just verify it parsed as canonical.
        assert!(uuid::Uuid::parse_str(out.uuid.as_str()).is_ok());
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn luks_uuid_rejects_invalid_uuid() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: "not-a-uuid\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidText { .. }));
    }

    #[test]
    fn luks_uuid_errors_on_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: String::new(),
            stderr: "Device /dev/vdz does not exist.".into(),
            exit_status: 5,
        };
        let err = parse_cryptsetup_luks_uuid(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn luks_uuid_classifies_not_luks_error() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: String::new(),
            stderr: "Device /dev/vdz is not a valid LUKS device.\n".into(),
            exit_status: 1,
        };
        assert!(cryptsetup_luks_uuid_reports_not_luks(&raw));
    }

    // Intent: an uppercase hyphenated UUID from `cryptsetup luksUUID`
    //   canonicalizes through LuksUuid::parse so equates with the
    //   lowercase form loaded from pool.json.
    // Why: Phase 1 left this site constructing LuksUuid(trimmed.to_owned())
    //   directly, bypassing canonicalization. Every PoolDevice.luks_uuid
    //   value flowing through probe_pool depends on this parser; an
    //   uppercase or URN form from an upstream cryptsetup release would
    //   silently miss every membership.by_uuid lookup against a
    //   canonical-key map. Pinning the test here gates the regression at
    //   the producer.
    // Scenario: a cryptsetup release emits the uppercase hyphenated form
    //   (test seed 800: this is the parser-to-membership flow pin).
    #[test]
    fn luks_uuid_canonicalizes_uppercase_hyphenated() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: "8C78A966-EF17-4610-B835-5B376EF10B4E\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_uuid(&raw).unwrap();
        let canonical = LuksUuid::parse("8c78a966-ef17-4610-b835-5b376ef10b4e").unwrap();
        assert_eq!(out.uuid, canonical);
        assert_eq!(out.uuid.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }

    // Intent: parse_cryptsetup_luks_uuid_from_dump extracts the UUID
    //   field from a real luksDump text body and yields a canonical
    //   LuksUuid.
    // Why: discover runs this parser alongside parse_cryptsetup_luks_version
    //   and parse_cryptsetup_luks_label over the same CryptsetupLuksDumpText
    //   raw output. Pinning on the stable fixture catches drift in the
    //   `UUID:` line format across cryptsetup releases.
    // Scenario: discover reads the LUKS header dump of a braid-labeled
    //   disk during cold-disk pool reconstruction.
    #[test]
    fn luks_uuid_from_dump_parses_nixos_26_05_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: fixture("cryptsetup-luks-dump.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let uuid = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap();
        assert!(uuid::Uuid::parse_str(uuid.as_str()).is_ok());
    }

    // Intent: a luksDump body with no `UUID:` line surfaces as
    //   ParseError::MissingField naming the UUID field.
    // Why: discover folds the missing-field outcome into
    //   DiscoverWarning::LuksDumpUnparseable (the parser-drift bucket,
    //   matching the missing-`Version:` path); a regression that swallowed
    //   the missing field would silently drop the disk from discovery.
    #[test]
    fn luks_uuid_from_dump_returns_missing_field_when_absent() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\nVersion:       \t2\nLabel:         \tbraid-foo\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap_err();
        match err {
            ParseError::MissingField { field, .. } => {
                assert_eq!(field, "UUID");
            }
            other => panic!("expected MissingField UUID, got {other:?}"),
        }
    }

    // Intent: a luksDump body whose `UUID:` value fails LuksUuid::parse
    //   surfaces as ParseError::InvalidValue naming the UUID field.
    // Why: discover maps the invalid-value outcome to
    //   DiscoverWarning::InvalidLuksUuid carrying the offending raw text.
    #[test]
    fn luks_uuid_from_dump_returns_invalid_value_when_unparseable() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\nVersion:       \t2\nUUID:          \tnot-a-uuid\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap_err();
        match err {
            ParseError::InvalidValue {
                field, raw, detail, ..
            } => {
                assert_eq!(field, "UUID");
                assert_eq!(raw, "not-a-uuid");
                assert!(!detail.is_empty(), "detail must carry uuid-crate reason");
            }
            other => panic!("expected InvalidValue UUID, got {other:?}"),
        }
    }

    // Intent: a UUID: line whose value contains the literal " ("
    //   substring yields structured raw/detail fields with no
    //   string-round-trip corruption.
    // Why it exists: an earlier implementation packed raw+detail into a
    //   single formatted string ("<raw> (<detail>)") and discover
    //   reverse-split it on " ("; any raw containing " (" silently
    //   truncated at the first match.
    // Scenario: a corrupted or hand-edited LUKS2 header dump line of the
    //   form "UUID:          \tnot (a uuid)\n" -- the " (" between
    //   "not" and "(a uuid)" is exactly the delimiter the old split
    //   matched first.
    #[test]
    fn luks_uuid_from_dump_preserves_delimiter_bearing_raw() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\nVersion:       \t2\nUUID:          \tnot (a uuid)\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap_err();
        match err {
            ParseError::InvalidValue {
                field, raw, detail, ..
            } => {
                assert_eq!(field, "UUID");
                assert_eq!(raw, "not (a uuid)");
                assert!(!detail.is_empty(), "detail must carry uuid-crate reason");
            }
            other => panic!("expected InvalidValue UUID, got {other:?}"),
        }
    }

    // Intent: a `UUID:` line carrying cryptsetup's literal `(no UUID)`
    //   sentinel surfaces as ParseError::InvalidValue with the sentinel
    //   preserved verbatim in `raw`.
    // Why: LUKS2_hdr_dump prints `(no UUID)` for an empty in-memory UUID
    //   field
    //   (reference/cryptsetup/lib/luks2/luks2_json_metadata.c#LUKS2_hdr_dump),
    //   so this pins the exact real-cryptsetup sentinel at the producing
    //   parser. The other InvalidValue tests feed `not-a-uuid` /
    //   `not (a uuid)`, never the sentinel discover routes to
    //   DiscoverWarning::InvalidLuksUuid.
    #[test]
    fn luks_uuid_from_dump_rejects_no_uuid_sentinel() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\nVersion:       \t2\nUUID:          \t(no UUID)\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap_err();
        match err {
            ParseError::InvalidValue {
                field, raw, detail, ..
            } => {
                assert_eq!(field, "UUID");
                assert_eq!(raw, "(no UUID)");
                assert!(!detail.is_empty(), "detail must carry uuid-crate reason");
            }
            other => panic!("expected InvalidValue UUID, got {other:?}"),
        }
    }

    // Intent: uppercase hyphenated UUID in a luksDump body canonicalizes
    //   on the dump-parser path identically to the luksUUID command path.
    // Why: discover routes through parse_cryptsetup_luks_uuid_from_dump,
    //   so the same canonicalization invariant must hold across both
    //   producers.
    #[test]
    fn luks_uuid_from_dump_canonicalizes_uppercase() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\n\
                     Version:       \t2\n\
                     UUID:          \t8C78A966-EF17-4610-B835-5B376EF10B4E\n\
                     Label:         \tbraid-foo\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let uuid = parse_cryptsetup_luks_uuid_from_dump(&raw).unwrap();
        assert_eq!(uuid.as_str(), "8c78a966-ef17-4610-b835-5b376ef10b4e");
    }
}
