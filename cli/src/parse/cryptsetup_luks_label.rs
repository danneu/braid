use crate::cmd::RawCommandOutput;

use super::types::CryptsetupLuksLabelOutput;
use super::ParseError;

/// Parse the LUKS label from `cryptsetup luksDump` text output.
///
/// The text output includes a `Label:` line:
/// ```text
/// LUKS header information
/// Version:        2
/// ...
/// Label:          braid-disk1
/// ...
/// ```
///
/// Returns `None` if the label is `(no label)` or empty.
pub fn parse_cryptsetup_luks_label(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksLabelOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let label = raw
        .stdout
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed.strip_prefix("Label:").map(|rest| {
                let value = rest.trim();
                if value.is_empty() || value == "(no label)" {
                    None
                } else {
                    Some(value.to_owned())
                }
            })
        })
        .flatten();

    Ok(CryptsetupLuksLabelOutput { label })
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

    #[test]
    fn extracts_braid_label() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             Epoch:         \t5\n\
             Metadata area: \t16384 [bytes]\n\
             Keyslots area: \t16744448 [bytes]\n\
             UUID:          \ta1b2c3d4-e5f6-7890-abcd-ef1234567890\n\
             Label:         \tbraid-disk1\n\
             Subsystem:     \t(no subsystem)\n",
        );
        let out = parse_cryptsetup_luks_label(&raw).unwrap();
        assert_eq!(out.label, Some("braid-disk1".to_owned()));
    }

    #[test]
    fn returns_none_for_no_label() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             Label:         \t(no label)\n\
             Subsystem:     \t(no subsystem)\n",
        );
        let out = parse_cryptsetup_luks_label(&raw).unwrap();
        assert_eq!(out.label, None);
    }

    #[test]
    fn returns_none_for_empty_label() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             Label:         \t\n\
             Subsystem:     \t(no subsystem)\n",
        );
        let out = parse_cryptsetup_luks_label(&raw).unwrap();
        assert_eq!(out.label, None);
    }

    #[test]
    fn returns_none_when_no_label_line() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             UUID:          \ta1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
        );
        let out = parse_cryptsetup_luks_label(&raw).unwrap();
        assert_eq!(out.label, None);
    }

    #[test]
    fn rejects_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump /dev/vda".into(),
            stdout: String::new(),
            stderr: "Device /dev/vda does not exist.".into(),
            exit_status: 5,
        };
        let err = parse_cryptsetup_luks_label(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn extracts_non_braid_label() {
        let raw = ok_raw(
            "LUKS header information\n\
             Version:       \t2\n\
             Label:         \tmy-other-thing\n",
        );
        let out = parse_cryptsetup_luks_label(&raw).unwrap();
        assert_eq!(out.label, Some("my-other-thing".to_owned()));
    }
}
