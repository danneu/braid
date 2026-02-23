use regex::Regex;

use crate::cmd::RawCommandOutput;
use crate::types::LuksUuid;

use super::types::{
    BtrfsDeviceStatsOutput, BtrfsFilesystemShowOutput, BtrfsFilesystemUsageOutput,
    BtrfsScrubStatusOutput, BtrfsShowDevice, CryptsetupLuksUuidOutput, CryptsetupStatusOutput,
    DeviceErrorStats, LsblkFieldOutput, ScrubState, ScrubTimestamp,
};
use super::ParseError;

// --- 1. parse_lsblk_field ---

pub fn parse_lsblk_field(raw: &RawCommandOutput) -> Result<LsblkFieldOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let trimmed = raw.stdout.trim();
    let value = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    };

    Ok(LsblkFieldOutput { value })
}

// --- 2. parse_cryptsetup_luks_uuid ---

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

    // Validate UUID format using uuid crate
    uuid::Uuid::parse_str(trimmed).map_err(|_| ParseError::InvalidText {
        cmd: raw.cmd.clone(),
        detail: format!("not a valid UUID: {trimmed:?}"),
    })?;

    Ok(CryptsetupLuksUuidOutput {
        uuid: LuksUuid(trimmed.to_owned()),
    })
}

// --- 3. parse_cryptsetup_status ---

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

// --- 4. parse_btrfs_scrub_status ---

pub fn parse_btrfs_scrub_status(
    raw: &RawCommandOutput,
) -> Result<BtrfsScrubStatusOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    // "no stats available" means scrub has never run
    if stdout.contains("no stats available") {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Never,
        });
    }

    // Look for "Scrub started:" line with a timestamp
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(timestamp) = trimmed.strip_prefix("Scrub started:") {
            let ts = timestamp.trim();
            if !ts.is_empty() && !ts.contains("not available") {
                return Ok(BtrfsScrubStatusOutput {
                    state: ScrubState::Completed {
                        started_at: ScrubTimestamp(ts.to_owned()),
                    },
                });
            }
        }
    }

    Ok(BtrfsScrubStatusOutput {
        state: ScrubState::Unknown,
    })
}

// --- 5. parse_btrfs_filesystem_usage ---

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

// --- 6. parse_btrfs_device_stats ---

pub fn parse_btrfs_device_stats(
    raw: &RawCommandOutput,
) -> Result<BtrfsDeviceStatsOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let re = Regex::new(r"^\[([^\]]+)\]\.(\S+)\s+(\d+)$").unwrap();

    // Collect stats keyed by device path, preserving order
    let mut device_order: Vec<String> = Vec::new();
    let mut stats_map: std::collections::HashMap<String, DeviceErrorStats> =
        std::collections::HashMap::new();

    for line in raw.stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let caps = re.captures(trimmed).ok_or_else(|| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: format!("unexpected device stats line: {trimmed:?}"),
        })?;

        let device_path = caps[1].to_owned();
        let field_name = &caps[2];
        let value: u64 = caps[3].parse().map_err(|_| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: format!("non-numeric stat value in: {trimmed:?}"),
        })?;

        let entry = stats_map.entry(device_path.clone()).or_insert_with(|| {
            device_order.push(device_path.clone());
            DeviceErrorStats {
                device_path: device_path.clone(),
                read_io_errs: 0,
                write_io_errs: 0,
                corruption_errs: 0,
                generation_errs: 0,
                flush_io_errs: 0,
            }
        });

        match field_name {
            "read_io_errs" => entry.read_io_errs = value,
            "write_io_errs" => entry.write_io_errs = value,
            "flush_io_errs" => entry.flush_io_errs = value,
            "corruption_errs" => entry.corruption_errs = value,
            "generation_errs" => entry.generation_errs = value,
            _ => {} // Ignore unknown fields — forward-compatible
        }
    }

    let devices = device_order
        .into_iter()
        .map(|path| stats_map.remove(&path).unwrap())
        .collect();

    Ok(BtrfsDeviceStatsOutput { devices })
}

// --- 7. parse_btrfs_filesystem_show ---

pub fn parse_btrfs_filesystem_show(
    raw: &RawCommandOutput,
) -> Result<BtrfsFilesystemShowOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let total_re = Regex::new(r"Total devices\s+(\d+)").unwrap();
    let devid_re = Regex::new(r"devid\s+(\d+)\s+.*path\s+(.+)").unwrap();

    let stdout = &raw.stdout;

    let total_devices = total_re
        .captures(stdout)
        .and_then(|c| c[1].parse::<u64>().ok())
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "Total devices".into(),
        })?;

    let devices: Vec<BtrfsShowDevice> = devid_re
        .captures_iter(stdout)
        .map(|c| BtrfsShowDevice {
            devid: c[1].parse().unwrap(),
            path: c[2].trim().to_owned(),
        })
        .collect();

    let has_missing = stdout.contains("Some devices missing");

    Ok(BtrfsFilesystemShowOutput {
        total_devices,
        devices,
        has_missing,
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

    // --- parse_lsblk_field ---

    #[test]
    fn lsblk_field_extracts_value() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  Samsung SSD 870  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_field(&raw).unwrap();
        assert_eq!(out.value.as_deref(), Some("Samsung SSD 870"));
    }

    #[test]
    fn lsblk_field_returns_none_for_empty() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_field(&raw).unwrap();
        assert_eq!(out.value, None);
    }

    #[test]
    fn lsblk_field_errors_on_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: String::new(),
            stderr: "not a block device".into(),
            exit_status: 32,
        };
        let err = parse_lsblk_field(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    // --- parse_cryptsetup_luks_uuid ---

    #[test]
    fn luks_uuid_parses_valid_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: fixture("cryptsetup-luks-uuid.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_uuid(&raw).unwrap();
        assert_eq!(out.uuid.0, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
    }

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

    // --- parse_cryptsetup_status ---

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

    // --- parse_btrfs_scrub_status ---

    #[test]
    fn scrub_never_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: fixture("btrfs-scrub-never.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Never);
    }

    #[test]
    fn scrub_completed_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: fixture("btrfs-scrub-completed.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Completed { started_at } => {
                assert!(started_at.0.contains("Mon Jan  6"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn scrub_unknown_on_empty_output() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Unknown);
    }

    // --- parse_btrfs_filesystem_usage ---

    #[test]
    fn usage_parses_raw_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: fixture("btrfs-usage-raw.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_usage(&raw).unwrap();
        assert_eq!(out.device_size_bytes, 21474836480);
        assert_eq!(out.used_bytes, 8589934592);
        assert_eq!(out.free_estimated_bytes, 6442450944);
    }

    #[test]
    fn usage_rejects_bad_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem usage".into(),
            stdout: fixture("btrfs-usage-bad.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_filesystem_usage(&raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingField { .. }));
    }

    // --- parse_btrfs_device_stats ---

    #[test]
    fn device_stats_parses_2disk_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: fixture("btrfs-device-stats-2disk.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices.len(), 2);
        assert_eq!(out.devices[0].device_path, "/dev/mapper/braid-vda");
        assert_eq!(out.devices[0].read_io_errs, 0);
        assert_eq!(out.devices[1].device_path, "/dev/mapper/braid-vdb");
    }

    #[test]
    fn device_stats_parses_errors_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: fixture("btrfs-device-stats-errors.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices[0].read_io_errs, 3);
        assert_eq!(out.devices[0].corruption_errs, 1);
        assert_eq!(out.devices[1].read_io_errs, 0);
    }

    /// Unknown fields from future btrfs-progs versions are silently ignored.
    /// Known fields still parse correctly. See cli/docs/command-capabilities.md.
    #[test]
    fn device_stats_ignores_unknown_fields_parses_known() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: "[/dev/mapper/braid-vda].write_io_errs    0\n\
                     [/dev/mapper/braid-vda].read_io_errs     2\n\
                     [/dev/mapper/braid-vda].flush_io_errs    0\n\
                     [/dev/mapper/braid-vda].corruption_errs  0\n\
                     [/dev/mapper/braid-vda].generation_errs  0\n\
                     [/dev/mapper/braid-vda].discard_errs     7\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].read_io_errs, 2);
        assert_eq!(out.devices[0].write_io_errs, 0);
        // discard_errs (unknown) is silently dropped — not in DeviceErrorStats
    }

    #[test]
    fn device_stats_empty_output_gives_no_devices() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert!(out.devices.is_empty());
    }

    // --- parse_btrfs_filesystem_show ---

    #[test]
    fn btrfs_show_parses_3disk_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-3disk.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 3);
        assert_eq!(out.devices.len(), 3);
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].path, "/dev/mapper/braid-vda");
        assert_eq!(out.devices[2].devid, 3);
        assert!(!out.has_missing);
    }

    #[test]
    fn btrfs_show_detects_degraded_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-degraded.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 1); // only 1 device listed, other is missing
        assert!(out.has_missing);
    }

    #[test]
    fn btrfs_show_rejects_bad_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-bad.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_filesystem_show(&raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingField { .. }));
    }
}
