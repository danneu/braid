use nom::{
    IResult,
    bytes::complete::{tag, take_till1},
    character::complete::{not_line_ending, space1},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsFilesystemUsageOutput, DataRatio};

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

// Parses an indented key-value line from the "Overall:" section:
//   "    Device size:\t\t\t1040187392"  →  ("Device size", "1040187392")
fn parse_kv_line(input: &str) -> IResult<&str, (&str, &str)> {
    let (input, _) = space1(input)?;
    let (input, key) = take_till1(|c| c == ':')(input)?;
    let (input, _) = tag(":")(input)?;
    let (input, _) = space1(input)?;
    let (input, value) = take_till1(|c: char| c.is_ascii_whitespace())(input)?;
    let (input, _) = not_line_ending(input)?;
    Ok((input, (key.trim(), value)))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_btrfs_filesystem_usage(
    raw: &RawCommandOutput,
) -> Result<BtrfsFilesystemUsageOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let mut device_size: Option<u64> = None;
    let mut used: Option<u64> = None;
    let mut free_est: Option<u64> = None;
    let mut data_ratio: Option<DataRatio> = None;

    for line in raw.stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains(':') {
            continue;
        }

        if let Ok((_, (key, value_str))) = parse_kv_line(line) {
            match key {
                "Device size" => device_size = value_str.parse().ok(),
                "Used" if used.is_none() => used = value_str.parse().ok(),
                "Free (estimated)" => free_est = value_str.parse().ok(),
                "Data ratio" => {
                    data_ratio = Some(DataRatio::parse(value_str).ok_or_else(|| {
                        ParseError::InvalidText {
                            cmd: raw.cmd.clone(),
                            detail: format!(
                                "Data ratio {value_str:?} is not in expected X.YY format"
                            ),
                        }
                    })?);
                }
                _ => {}
            }
        }
    }

    let device_size = device_size.ok_or_else(|| ParseError::MissingField {
        cmd: raw.cmd.clone(),
        field: "Device size".into(),
    })?;
    let used = used.ok_or_else(|| ParseError::MissingField {
        cmd: raw.cmd.clone(),
        field: "Used".into(),
    })?;
    let free_est = free_est.ok_or_else(|| ParseError::MissingField {
        cmd: raw.cmd.clone(),
        field: "Free (estimated)".into(),
    })?;
    let data_ratio = data_ratio.ok_or_else(|| ParseError::MissingField {
        cmd: raw.cmd.clone(),
        field: "Data ratio".into(),
    })?;

    Ok(BtrfsFilesystemUsageOutput {
        device_size_bytes: device_size,
        used_bytes: used,
        free_estimated_bytes: free_est,
        data_ratio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::types::DataRatio;
    use crate::test_fixtures::read_stable_fixture as fixture;

    // --- Contract tests (nixos-26.05 fixtures) ---

    #[test]
    fn usage_parses_nixos_26_05() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: fixture("btrfs-usage-raw.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.device_size_bytes, 2113929216);
        assert_eq!(out.used_bytes, 33947648);
        assert_eq!(out.free_estimated_bytes, 926154752);
        assert_eq!(out.data_ratio, DataRatio::parse("2.00").unwrap());
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn usage_rejects_malformed_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: "Overall:\n    Some random line\n    No device size here\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_filesystem_usage(&raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingField { .. }));
    }

    #[test]
    fn usage_parses_single_data_ratio() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: "Overall:\n\
                     \tDevice size:\t\t\t1040187392\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\n\
                     \tData ratio:\t\t\t1.00\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.data_ratio, DataRatio::parse("1.00").unwrap());
    }

    #[test]
    fn usage_rejects_invalid_data_ratio_format() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: "Overall:\n\
                     \tDevice size:\t\t\t1040187392\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\n\
                     \tData ratio:\t\t\t1.001\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_filesystem_usage(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidText { ref detail, .. } if detail.contains("1.001")),
            "expected InvalidText mentioning 1.001, got: {err:?}"
        );
    }

    #[test]
    fn usage_parses_one_frac_digit_data_ratio() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: "Overall:\n\
                     \tDevice size:\t\t\t1040187392\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\n\
                     \tData ratio:\t\t\t1.0\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.data_ratio, DataRatio::parse("1.00").unwrap());
    }

    #[test]
    fn usage_parses_intermediate_data_ratio() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: "Overall:\n\
                     \tDevice size:\t\t\t1040187392\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\n\
                     \tData ratio:\t\t\t1.01\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.data_ratio, DataRatio::parse("1.01").unwrap());
    }
}
