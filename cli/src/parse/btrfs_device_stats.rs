use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::types::{BtrfsDeviceStatsOutput, DeviceErrorStats, DeviceStatsTarget};
use super::ParseError;

// --- Serde helper structs (not exposed to domain code) ---

#[derive(Deserialize)]
struct RawDeviceStatsOutput {
    #[serde(rename = "__header")]
    _header: Option<serde_json::Value>,

    #[serde(rename = "device-stats")]
    device_stats: Vec<RawDeviceStatsEntry>,
}

// No deny_unknown_fields: forward-compatible with future fields like discard_errs.
#[derive(Deserialize)]
struct RawDeviceStatsEntry {
    device: String,
    #[allow(dead_code)]
    devid: u64,
    write_io_errs: u64,
    read_io_errs: u64,
    flush_io_errs: u64,
    corruption_errs: u64,
    generation_errs: u64,
}

// --- Public parse function ---

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

    let parsed: RawDeviceStatsOutput =
        serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: e.to_string(),
        })?;

    let devices = parsed
        .device_stats
        .into_iter()
        .map(|e| {
            // Missing-device sentinel handling.
            //
            // Observed degraded-mount behavior in braid's VM test:
            // `btrfs --format json device stats` reports `"<missing disk>"` for the
            // missing member. That is the case that caused `braid monitor` to misparse
            // the row as a normal path and fail with `UnmappedDeviceError`.
            //
            // In the btrfs-progs source I checked (v6.19-5-g10717dd7), there is also a
            // separate fallback that synthesizes `"devid:<n>"` when
            // `path_canonicalize(path)` returns NULL. That fallback does not explain the
            // degraded-mount `"<missing disk>"` output; it is a different code path.
            // Accept both forms.
            let target = if e.device.starts_with("devid:") || e.device == "<missing disk>" {
                DeviceStatsTarget::MissingDisk
            } else {
                DeviceStatsTarget::Path(e.device)
            };
            DeviceErrorStats {
                target,
                read_io_errs: e.read_io_errs,
                write_io_errs: e.write_io_errs,
                flush_io_errs: e.flush_io_errs,
                corruption_errs: e.corruption_errs,
                generation_errs: e.generation_errs,
            }
        })
        .collect();

    Ok(BtrfsDeviceStatsOutput { devices })
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

    fn is_dm_or_mapper_path(s: &str) -> bool {
        s.starts_with("/dev/dm-") || s.starts_with("/dev/mapper/braid-")
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn device_stats_parses_nixos_25_11_2disk() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: fixture("btrfs-device-stats-2disk.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices.len(), 2);
        assert!(
            is_dm_or_mapper_path(out.devices[0].target.as_path().unwrap()),
            "device 0 path should be dm or mapper, got: {:?}",
            out.devices[0].target
        );
        assert_eq!(out.devices[0].read_io_errs, 0);
        assert!(
            is_dm_or_mapper_path(out.devices[1].target.as_path().unwrap()),
            "device 1 path should be dm or mapper, got: {:?}",
            out.devices[1].target
        );
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn device_stats_parses_errors_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{
                "device-stats": [
                    {
                        "device": "/dev/mapper/braid-vda",
                        "devid": 1,
                        "write_io_errs": 0,
                        "read_io_errs": 3,
                        "flush_io_errs": 0,
                        "corruption_errs": 1,
                        "generation_errs": 0
                    },
                    {
                        "device": "/dev/mapper/braid-vdb",
                        "devid": 2,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    }
                ]
            }"#
            .into(),
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
            stdout: r#"{
                "device-stats": [
                    {
                        "device": "/dev/mapper/braid-vda",
                        "devid": 1,
                        "write_io_errs": 0,
                        "read_io_errs": 2,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0,
                        "discard_errs": 7
                    }
                ]
            }"#
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

    /// Observed degraded-mount sentinel: btrfs-progs emits "<missing disk>"
    /// as the device path for absent drives.
    #[test]
    fn device_stats_parses_observed_missing_disk_sentinel() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{
                "device-stats": [
                    {
                        "device": "/dev/mapper/braid-vda",
                        "devid": 1,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    },
                    {
                        "device": "<missing disk>",
                        "devid": 2,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    }
                ]
            }"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices.len(), 2);
        assert_eq!(
            out.devices[0].target,
            DeviceStatsTarget::Path("/dev/mapper/braid-vda".to_owned())
        );
        assert_eq!(out.devices[1].target, DeviceStatsTarget::MissingDisk);
    }

    /// Separate upstream fallback case: in btrfs-progs v6.19-5-g10717dd7,
    /// `"devid:<n>"` is synthesized when `path_canonicalize(path)` returns NULL.
    /// This is not the observed degraded-mount sentinel in braid's failing VM
    /// test, but we accept it defensively.
    #[test]
    fn device_stats_parses_upstream_devid_fallback_sentinel() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{
                "device-stats": [
                    {
                        "device": "devid:2",
                        "devid": 2,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    }
                ]
            }"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].target, DeviceStatsTarget::MissingDisk);
    }

    #[test]
    fn device_stats_empty_output_gives_no_devices() {
        let raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{"device-stats": []}"#.into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_stats(&raw).unwrap();
        assert!(out.devices.is_empty());
    }
}
