use regex::Regex;

use crate::cmd::RawCommandOutput;

use super::types::{BtrfsDeviceStatsOutput, DeviceErrorStats};
use super::ParseError;

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
}
