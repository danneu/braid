use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, take_till1},
    character::complete::{not_line_ending, space0},
    combinator::eof,
};

use crate::cmd::RawCommandOutput;
use crate::types::BackingPath;

use super::ParseError;
use super::types::{BackingDevice, CryptsetupStatusOutput};

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
        return Ok(CryptsetupStatusOutput::Inactive);
    }

    if raw.exit_status != 0 {
        let stderr = raw.stderr.trim();
        // Non-zero exit is expected when device is not active.
        // Benign if stderr is empty or matches structured "not active" message.
        if stderr.is_empty() || parse_inactive_message(stderr).is_ok() {
            return Ok(CryptsetupStatusOutput::Inactive);
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
            parse_device_line(line.trim())
                .ok()
                .map(|(_, v)| v.to_owned())
        })
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "device".into(),
        })?;

    let backing = if device.is_empty() || device == "(null)" {
        BackingDevice::Null
    } else {
        BackingDevice::Path(
            BackingPath::parse(&device).map_err(|e| ParseError::InvalidValue {
                cmd: raw.cmd.clone(),
                field: "device".into(),
                raw: e.raw,
                detail: e.detail,
            })?,
        )
    };
    Ok(CryptsetupStatusOutput::Active { backing })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::read_stable_fixture as fixture;

    #[test]
    fn cryptsetup_status_parses_active_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: fixture("cryptsetup-status-active.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_status(&raw).unwrap();
        assert!(matches!(
            out,
            CryptsetupStatusOutput::Active { backing: BackingDevice::Path(p) } if p.as_str() == "/dev/vdb"
        ));
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
        assert_eq!(out, CryptsetupStatusOutput::Inactive);
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

    #[test]
    fn cryptsetup_status_active_with_null_backing_collapses_to_null() {
        let null_literal = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: "/dev/mapper/braid-vda is active.\n  device:  (null)\n  type:  LUKS2\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let whitespace_only = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: "/dev/mapper/braid-vda is active.\n  device:    \n  type:  LUKS2\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        assert_eq!(
            parse_cryptsetup_status(&null_literal).unwrap(),
            CryptsetupStatusOutput::Active {
                backing: BackingDevice::Null
            }
        );
        assert_eq!(
            parse_cryptsetup_status(&whitespace_only).unwrap(),
            CryptsetupStatusOutput::Active {
                backing: BackingDevice::Null
            }
        );
    }

    // Intent: invalid active `device:` values surface as structured
    //   ParseError::InvalidValue for the device field.
    // Why it exists: cryptsetup status is the single backing-path parse
    //   boundary; consumers must never re-spend a malformed status value as
    //   `cryptsetup luksUUID <device>`.
    // Scenario: cryptsetup output unexpectedly reports a non-absolute device
    //   path and the parser rejects it before command planning continues.
    #[test]
    fn cryptsetup_status_invalid_device_is_invalid_value() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup status".into(),
            stdout: "/dev/mapper/braid-vda is active.\n  device:  dev/vda\n  type:  LUKS2\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_status(&raw).unwrap_err();
        match err {
            ParseError::InvalidValue {
                field, raw, detail, ..
            } => {
                assert_eq!(field, "device");
                assert_eq!(raw, "dev/vda");
                assert_eq!(detail, "must be an absolute path");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }
}
