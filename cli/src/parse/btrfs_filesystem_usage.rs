use nom::{
    bytes::complete::{tag, take_till1},
    character::complete::{not_line_ending, space1},
    IResult,
};

use crate::cmd::RawCommandOutput;

use super::types::BtrfsFilesystemUsageOutput;
use super::ParseError;

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
    let mut data_ratio: Option<u64> = None;

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
                    // braid only supports RAID1 (data ratio 2). Reject anything else
                    // so we get a clear error rather than silently computing wrong capacity.
                    if value_str == "2.00" {
                        data_ratio = Some(2);
                    } else {
                        return Err(ParseError::InvalidText {
                            cmd: raw.cmd.clone(),
                            detail: format!(
                                "unsupported Data ratio {value_str:?} (expected \"2.00\" for RAID1)"
                            ),
                        });
                    }
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

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-25.11/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn usage_parses_nixos_25_11() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: fixture("btrfs-usage-raw.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.device_size_bytes, 1040187392);
        assert_eq!(out.used_bytes, 33914880);
        assert_eq!(out.free_estimated_bytes, 442957824);
        assert_eq!(out.data_ratio, 2);
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
    fn usage_rejects_unsupported_data_ratio() {
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
        let err = parse_btrfs_filesystem_usage(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidText { ref detail, .. } if detail.contains("1.00")),
            "expected InvalidText mentioning 1.00, got: {err:?}"
        );
    }
}
