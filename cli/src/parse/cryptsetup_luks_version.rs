use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::CryptsetupLuksVersionOutput;

/// Parse the LUKS version from `cryptsetup luksDump` text output.
///
/// The text output begins with:
/// ```text
/// LUKS header information
/// Version:        2
/// ```
/// Both LUKS1 and LUKS2 emit a `Version:` line in this format
/// (`reference/cryptsetup/lib/setup.c:6138`,
///  `reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2198`).
pub fn parse_cryptsetup_luks_version(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksVersionOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let version_str = raw
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Version:").map(str::trim))
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "Version".into(),
        })?;

    let version: u32 = version_str
        .parse()
        .map_err(|_| ParseError::UnexpectedValue {
            cmd: raw.cmd.clone(),
            field: "Version".into(),
            value: version_str.to_owned(),
        })?;

    Ok(CryptsetupLuksVersionOutput { version })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_raw(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup luksDump /dev/vda".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /*
     * Intent: parse a LUKS2 header dump and extract version 2.
     * Why it exists: probe_config_disk and discover.rs both gate on
     *   version == 2 to enforce braid's LUKS2-only invariant. If this
     *   parser ever fails to recognize a healthy LUKS2 dump, every
     *   probe in the codebase would falsely reject good disks.
     * Scenario: the typical output from `cryptsetup luksDump` on a
     *   braid-formatted device.
     */
    #[test]
    fn parses_luks2_version() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             Epoch:         \t5\n\
             Metadata area: \t16384 [bytes]\n\
             Keyslots area: \t16744448 [bytes]\n\
             UUID:          \ta1b2c3d4-e5f6-7890-abcd-ef1234567890\n\
             Label:         \tbraid-disk1\n",
        );
        let out = parse_cryptsetup_luks_version(&raw).unwrap();
        assert_eq!(out.version, 2);
    }

    /*
     * Intent: parse a LUKS1 header dump and extract version 1.
     * Why it exists: this is the wrong-version case the gateway
     *   probes use to surface "braid requires LUKS2" errors. If the
     *   parser silently coerced LUKS1 to 2 (or failed), the gateway
     *   would lie to the user.
     * Scenario: a disk formatted with `cryptsetup luksFormat --type luks1`
     *   that somehow ends up in front of braid.
     */
    #[test]
    fn parses_luks1_version() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t1\n\
             Cipher name:   \taes\n\
             Cipher mode:   \txts-plain64\n\
             Hash spec:     \tsha256\n",
        );
        let out = parse_cryptsetup_luks_version(&raw).unwrap();
        assert_eq!(out.version, 1);
    }

    /*
     * Intent: a non-zero exit from luksDump must surface as
     *   ParseError::CommandFailed with the original stderr preserved.
     * Why it exists: the probe-layer logic distinguishes "command
     *   failed" from "no Version field present"; both should not
     *   collapse into one variant.
     * Scenario: cryptsetup binary refuses to dump a damaged or
     *   non-LUKS device and exits non-zero.
     */
    #[test]
    fn errors_on_command_failure() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump /dev/vda".into(),
            stdout: String::new(),
            stderr: "Device /dev/vda is not a valid LUKS device.\n".into(),
            exit_status: 1,
        };
        let err = parse_cryptsetup_luks_version(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    /*
     * Intent: stdout that does not contain a `Version:` line must
     *   surface as ParseError::MissingField.
     * Why it exists: defensive against future cryptsetup output
     *   changes that drop or rename the Version field — we want a
     *   loud parse error, not a silent default.
     * Scenario: hypothetical cryptsetup output drift.
     */
    #[test]
    fn errors_on_missing_version_field() {
        let raw = ok_raw("LUKS header information\nUUID: foo\n");
        let err = parse_cryptsetup_luks_version(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField { ref field, .. } if field == "Version"),
            "expected MissingField {{ Version }}, got: {err:?}"
        );
    }

    /*
     * Intent: a non-integer Version value must surface as
     *   ParseError::UnexpectedValue with the offending text.
     * Why it exists: garbled output should fail the probe loudly,
     *   not coerce to 0 or panic.
     * Scenario: hypothetical garbled stdout from a bad pipe or
     *   corrupted output.
     */
    #[test]
    fn errors_on_non_integer_version() {
        let raw = ok_raw("LUKS header information\nVersion: abc\n");
        let err = parse_cryptsetup_luks_version(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::UnexpectedValue { ref field, ref value, .. }
                if field == "Version" && value == "abc"),
            "expected UnexpectedValue {{ Version, abc }}, got: {err:?}"
        );
    }
}
