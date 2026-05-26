use nom::{
    IResult,
    bytes::complete::{tag, take_till1, take_until},
    character::complete::{i64 as parse_i64, not_line_ending, space0, space1, u64 as parse_u64},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsDeviceUsageEntry, BtrfsDeviceUsageOutput, DeviceAllocation};

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

// Parses device header: "/dev/mapper/braid-vdb, ID: 1"  →  ("/dev/mapper/braid-vdb", 1)
fn parse_device_header(input: &str) -> IResult<&str, (&str, u64)> {
    let (input, path) = take_until(",")(input)?;
    let (input, _) = tag(",")(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("ID:")(input)?;
    let (input, _) = space1(input)?;
    let (input, devid) = parse_u64(input)?;
    Ok((input, (path.trim(), devid)))
}

// Parses an indented key-value line: "   Device size:          536870912"  →  ("Device size", 536870912)
// Uses i64 internally because btrfs can report negative Unallocated during
// device removal (transient state). Negative values are clamped to 0.
fn parse_kv_line(input: &str) -> IResult<&str, (&str, u64)> {
    let (input, _) = space1(input)?;
    let (input, key) = take_till1(|c| c == ':')(input)?;
    let (input, _) = tag(":")(input)?;
    let (input, _) = space1(input)?;
    let (input, value) = parse_i64(input)?;
    let (input, _) = not_line_ending(input)?;
    Ok((input, (key.trim(), value.max(0) as u64)))
}

pub fn parse_btrfs_device_usage(
    raw: &RawCommandOutput,
) -> Result<BtrfsDeviceUsageOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let mut devices: Vec<BtrfsDeviceUsageEntry> = Vec::new();

    // Current device being built
    let mut current: Option<PartialDevice> = None;

    for line in raw.stdout.lines() {
        if line.trim().is_empty() {
            // Blank line — finalize current device if any
            if let Some(partial) = current.take() {
                devices.push(finalize_device(&raw.cmd, partial)?);
            }
            continue;
        }

        // Try parsing as device header (unindented line with ", ID:")
        if !line.starts_with(' ')
            && !line.starts_with('\t')
            && let Ok((_, (path, devid))) = parse_device_header(line)
        {
            // Finalize previous device if any
            if let Some(partial) = current.take() {
                devices.push(finalize_device(&raw.cmd, partial)?);
            }
            current = Some(PartialDevice {
                path: path.to_owned(),
                devid,
                device_size: None,
                device_slack: None,
                unallocated: None,
                allocations: Vec::new(),
            });
            continue;
        }

        // Try parsing as key-value line (indented)
        if let Some(ref mut dev) = current
            && let Ok((_, (key, value))) = parse_kv_line(line)
        {
            match key {
                "Device size" => dev.device_size = Some(value),
                "Device slack" => dev.device_slack = Some(value),
                "Unallocated" => dev.unallocated = Some(value),
                k if k.contains(',') => {
                    // Allocation line like "Data,RAID1" or "Metadata,RAID1"
                    if let Some((alloc_type, profile)) = k.split_once(',') {
                        dev.allocations.push(DeviceAllocation {
                            alloc_type: alloc_type.trim().to_owned(),
                            profile: profile.trim().to_owned(),
                            bytes: value,
                        });
                    }
                }
                _ => {
                    // Unknown key — silently ignored for forward-compatibility
                }
            }
        }
    }

    // Finalize last device
    if let Some(partial) = current.take() {
        devices.push(finalize_device(&raw.cmd, partial)?);
    }

    Ok(BtrfsDeviceUsageOutput { devices })
}

struct PartialDevice {
    path: String,
    devid: u64,
    device_size: Option<u64>,
    device_slack: Option<u64>,
    unallocated: Option<u64>,
    allocations: Vec<DeviceAllocation>,
}

fn finalize_device(cmd: &str, dev: PartialDevice) -> Result<BtrfsDeviceUsageEntry, ParseError> {
    let device_size = dev.device_size.ok_or_else(|| ParseError::MissingField {
        cmd: cmd.to_owned(),
        field: format!("Device size for {}", dev.path),
    })?;
    let device_slack = dev.device_slack.ok_or_else(|| ParseError::MissingField {
        cmd: cmd.to_owned(),
        field: format!("Device slack for {}", dev.path),
    })?;
    let unallocated = dev.unallocated.ok_or_else(|| ParseError::MissingField {
        cmd: cmd.to_owned(),
        field: format!("Unallocated for {}", dev.path),
    })?;

    Ok(BtrfsDeviceUsageEntry {
        path: dev.path,
        devid: dev.devid,
        device_size,
        device_slack,
        allocations: dev.allocations,
        unallocated,
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

    fn is_dm_or_mapper_path(s: &str) -> bool {
        s.starts_with("/dev/dm-") || s.starts_with("/dev/mapper/braid-")
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn device_usage_parses_nixos_25_11_2disk() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: fixture("btrfs-device-usage-2disk.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert_eq!(out.devices.len(), 2);
        assert!(
            is_dm_or_mapper_path(&out.devices[0].path),
            "devid 1 path should be dm or mapper, got: {}",
            out.devices[0].path
        );
        assert_eq!(out.devices[0].devid, 1);
        assert!(
            is_dm_or_mapper_path(&out.devices[1].path),
            "devid 2 path should be dm or mapper, got: {}",
            out.devices[1].path
        );
        assert_eq!(out.devices[1].devid, 2);
        assert!(out.devices[0].device_size > 0);
        assert!(out.devices[0].unallocated > 0);
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn device_usage_single_device() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vda, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,single:          67108864\n\
                     \x20  Metadata,DUP:         67108864\n\
                     \x20  System,single:        4194304\n\
                     \x20  Unallocated:          398458880\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].path, "/dev/mapper/braid-vda");
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].device_size, 536870912);
        assert_eq!(out.devices[0].device_slack, 0);
        assert_eq!(out.devices[0].unallocated, 398458880);
        assert_eq!(out.devices[0].allocations.len(), 3);
        assert_eq!(out.devices[0].allocations[0].alloc_type, "Data");
        assert_eq!(out.devices[0].allocations[0].profile, "single");
        assert_eq!(out.devices[0].allocations[0].bytes, 67108864);
        assert_eq!(out.devices[0].allocations[1].alloc_type, "Metadata");
        assert_eq!(out.devices[0].allocations[1].profile, "DUP");
    }

    #[test]
    // Intent: parse the current btrfs-progs missing-device marker from
    //   `device usage --raw`.
    // Why it exists: remove-missing relocation checks depend on `device_size
    //   == 0`, devid, allocations, and unallocated bytes surviving even when
    //   the path is the v6.17.1 `missing` marker.
    // Scenario: btrfs-progs `filesystem-usage.c:820-821` renders one live
    //   device plus one absent device as `missing, ID: 3`.
    fn device_usage_parses_missing_device_marker() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vda, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:                 0\n\
                     \x20  Data,RAID1:            67108864\n\
                     \x20  Unallocated:          469762048\n\n\
                     missing, ID: 3\n\
                     \x20  Device size:                  0\n\
                     \x20  Device slack:                 0\n\
                     \x20  Data,RAID1:            67108864\n\
                     \x20  Unallocated:            1234567\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };

        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert_eq!(out.devices.len(), 2);
        let missing = &out.devices[1];
        assert_eq!(missing.path, "missing");
        assert_eq!(missing.devid, 3);
        assert_eq!(missing.device_size, 0);
        assert_eq!(missing.device_slack, 0);
        assert_eq!(missing.unallocated, 1_234_567);
        assert_eq!(missing.allocations.len(), 1);
        assert_eq!(missing.allocations[0].alloc_type, "Data");
        assert_eq!(missing.allocations[0].profile, "RAID1");
        assert_eq!(missing.allocations[0].bytes, 67_108_864);
    }

    /// Unknown keys from future btrfs-progs versions are silently ignored.
    /// Known fields and allocations still parse correctly.
    /// See cli/docs/command-capabilities.md.
    #[test]
    fn device_usage_ignores_unknown_keys() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vda, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  FutureField:          999\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Unallocated:          398458880\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].device_size, 536870912);
        assert_eq!(out.devices[0].allocations.len(), 1);
        assert_eq!(out.devices[0].allocations[0].alloc_type, "Data");
        assert_eq!(out.devices[0].allocations[0].profile, "RAID1");
    }

    #[test]
    fn device_usage_used_bytes_helper() {
        let entry = BtrfsDeviceUsageEntry {
            path: "/dev/mapper/test".into(),
            devid: 1,
            device_size: 1000,
            device_slack: 0,
            allocations: vec![
                DeviceAllocation {
                    alloc_type: "Data".into(),
                    profile: "RAID1".into(),
                    bytes: 100,
                },
                DeviceAllocation {
                    alloc_type: "Metadata".into(),
                    profile: "RAID1".into(),
                    bytes: 50,
                },
                DeviceAllocation {
                    alloc_type: "System".into(),
                    profile: "RAID1".into(),
                    bytes: 10,
                },
            ],
            unallocated: 840,
        };
        assert_eq!(entry.used_bytes(), 160);
    }

    #[test]
    fn device_usage_used_bytes_empty() {
        let entry = BtrfsDeviceUsageEntry {
            path: "/dev/mapper/test".into(),
            devid: 1,
            device_size: 1000,
            device_slack: 0,
            allocations: vec![],
            unallocated: 1000,
        };
        assert_eq!(entry.used_bytes(), 0);
    }

    #[test]
    fn device_usage_error_exit() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        };
        let err = parse_btrfs_device_usage(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn device_usage_missing_required_field() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vda, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_device_usage(&raw).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField { ref field, .. } if field.contains("Unallocated")),
            "expected MissingField for Unallocated, got: {err:?}"
        );
    }

    #[test]
    fn device_usage_empty_output() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert!(out.devices.is_empty());
    }
}
