use crate::cmd::RawCommandOutput;

use super::types::BtrfsFilesystemUsageOutput;
use super::ParseError;

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

    for line in raw.stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Device size:") {
            device_size = parse_trailing_u64(rest);
        } else if trimmed.starts_with("Used:") && used.is_none() {
            if let Some(rest) = trimmed.strip_prefix("Used:") {
                used = parse_trailing_u64(rest);
            }
        } else if let Some(rest) = trimmed.strip_prefix("Free (estimated):") {
            free_est = parse_trailing_u64(rest);
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

    Ok(BtrfsFilesystemUsageOutput {
        device_size_bytes: device_size,
        used_bytes: used,
        free_estimated_bytes: free_est,
    })
}

/// Extract the first integer from a string like "  21474836480" or "  6442450944      (min: 6442450944)"
fn parse_trailing_u64(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
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
}
