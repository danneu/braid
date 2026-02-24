use nom::{
    branch::alt,
    bytes::complete::{tag, take_till1},
    character::complete::{not_line_ending, space0},
    combinator::eof,
    IResult, Parser,
};

use crate::cmd::RawCommandOutput;

use super::types::CryptsetupStatusOutput;
use super::ParseError;

// Parses: "  device:  /dev/vda"  →  "/dev/vda"
fn parse_device_line(input: &str) -> IResult<&str, &str> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("device:")(input)?;
    let (input, _) = space0(input)?;
    let (input, value) = not_line_ending(input)?;
    Ok((input, value.trim()))
}

// Parses inactive status lines from real-world cryptsetup output variants:
// - "/dev/mapper/braid-vdb is inactive."
// - "Device braid-vdb is not active."
fn parse_inactive_message(input: &str) -> IResult<&str, ()> {
    let (input, _) = alt((tag("Device "), tag("/dev/mapper/"))).parse(input)?;
    let (input, _) = take_till1(|c: char| c.is_ascii_whitespace())(input)?;
    let (input, _) = alt((tag(" is inactive."), tag(" is not active."))).parse(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = eof(input)?;
    Ok((input, ()))
}

pub fn parse_cryptsetup_status(
    raw: &RawCommandOutput,
) -> Result<CryptsetupStatusOutput, ParseError> {
    if parse_inactive_message(raw.stdout.trim()).is_ok()
        || parse_inactive_message(raw.stderr.trim()).is_ok()
    {
        return Ok(CryptsetupStatusOutput {
            is_active: false,
            device: None,
        });
    }

    if raw.exit_status != 0 {
        let stderr = raw.stderr.trim();
        // Non-zero exit is expected when device is not active.
        // Benign if stderr is empty or matches structured "not active" message.
        if stderr.is_empty() || parse_inactive_message(stderr).is_ok() {
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
        .find_map(|line| parse_device_line(line.trim()).ok().map(|(_, v)| v.to_owned()))
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
            "{}/tests/fixtures/nixos-25.11/{name}",
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
        assert_eq!(out.device.as_deref(), Some("/dev/vdb"));
    }

    #[test]
    fn cryptsetup_status_inactive_on_expected_stderr() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: fixture("cryptsetup-status-inactive.stdout"),
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
