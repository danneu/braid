use regex::Regex;

use crate::cmd::RawCommandOutput;

use super::types::{BtrfsFilesystemShowOutput, BtrfsShowDevice};
use super::ParseError;

// ---------------------------------------------------------------------------
// DeviceBtrfsProbe — classify raw btrfs-filesystem-show output
// ---------------------------------------------------------------------------

pub enum DeviceBtrfsProbe {
    HasBtrfs,
    NoBtrfs,
    Unknown(String),
}

/// Classify raw btrfs-filesystem-show output for a single device probe.
pub fn classify_btrfs_probe(raw: &RawCommandOutput) -> DeviceBtrfsProbe {
    if raw.exit_status == 0 {
        return DeviceBtrfsProbe::HasBtrfs;
    }
    let combined = format!("{} {}", raw.stdout, raw.stderr).to_lowercase();
    if combined.contains("not a valid btrfs filesystem")
        || combined.contains("no valid btrfs found")
        || combined.contains("no btrfs")
    {
        DeviceBtrfsProbe::NoBtrfs
    } else {
        DeviceBtrfsProbe::Unknown(format!(
            "btrfs filesystem show exit {}: {}",
            raw.exit_status,
            raw.stderr.trim()
        ))
    }
}

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

    // Filter out missing-device placeholders. btrfs-progs prints:
    //   devid  2 size 0 used 0 path /dev/mapper/disk-2 MISSING
    // These are synthetic — the device is gone. Only real present devices
    // are included; non-mapper real paths (e.g. /dev/sda1) are kept so
    // probe_pool can hard-fail on the invariant violation.
    let devices: Vec<BtrfsShowDevice> = devid_re
        .captures_iter(stdout)
        .filter_map(|c| {
            let path = c[2].trim();
            if path.ends_with(" MISSING") {
                None
            } else {
                Some(BtrfsShowDevice {
                    devid: c[1].parse().unwrap(),
                    path: path.to_owned(),
                })
            }
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

    /// btrfs-progs prints `path /dev/mapper/X MISSING` for gone devices.
    /// Parser excludes the placeholder; total_devices and has_missing are authoritative.
    #[test]
    fn btrfs_show_excludes_missing_sentinel_device() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-degraded-missing-line.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 1, "MISSING sentinel device must be excluded");
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].path, "/dev/mapper/braid-vda");
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

    // --- classify_btrfs_probe ---

    #[test]
    fn classify_btrfs_probe_exit_0_has_btrfs() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: abc-123\n\tTotal devices 2".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        assert!(matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::HasBtrfs));
    }

    #[test]
    fn classify_btrfs_probe_not_valid_filesystem() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: not a valid btrfs filesystem on /dev/dm-0".into(),
            exit_status: 1,
        };
        assert!(matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs));
    }

    #[test]
    fn classify_btrfs_probe_no_btrfs() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: no btrfs on /dev/dm-0".into(),
            exit_status: 1,
        };
        assert!(matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs));
    }

    #[test]
    fn classify_btrfs_probe_unknown_error() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: unexpected internal error".into(),
            exit_status: 1,
        };
        assert!(matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::Unknown(_)));
    }
}
