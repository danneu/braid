use crate::cmd::RawCommandOutput;

use super::types::CryptsetupStatusOutput;
use super::ParseError;

pub fn parse_cryptsetup_status(
    raw: &RawCommandOutput,
) -> Result<CryptsetupStatusOutput, ParseError> {
    if raw.exit_status != 0 {
        let stderr = raw.stderr.trim();
        // Non-zero exit is expected when device is not active.
        // Benign if stderr is empty or contains expected "not active" pattern.
        if stderr.is_empty() || stderr.contains("is not active") {
            return Ok(CryptsetupStatusOutput {
                is_active: false,
                device: None,
            });
        }
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    // Extract "device:" line value — required when active
    let device = raw
        .stdout
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("device:")
                .map(|v| v.trim().to_owned())
        })
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "device".into(),
        })?;

    Ok(CryptsetupStatusOutput {
        is_active: true,
        device: Some(device),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/phase2/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn cryptsetup_status_parses_active_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: fixture("cryptsetup-status-active.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_status(&raw).unwrap();
        assert!(out.is_active);
        assert_eq!(out.device.as_deref(), Some("/dev/vda"));
    }

    #[test]
    fn cryptsetup_status_inactive_on_expected_stderr() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: String::new(),
            stderr: fixture("cryptsetup-status-inactive.stderr"),
            exit_status: 4,
        };
        let out = parse_cryptsetup_status(&raw).unwrap();
        assert!(!out.is_active);
        assert_eq!(out.device, None);
    }

    #[test]
    fn cryptsetup_status_errors_on_unexpected_stderr() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: String::new(),
            stderr: "Cannot use device /dev/vda which is in use (already mapped or mounted).\n"
                .into(),
            exit_status: 5,
        };
        let err = parse_cryptsetup_status(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn cryptsetup_status_errors_when_active_but_no_device_line() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: "/dev/mapper/braid-vda is active and is in use.\n  type:    LUKS2\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_status(&raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingField { .. }));
    }
}
