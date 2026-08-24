use nom::{
    IResult,
    bytes::complete::{tag, take_till1, take_until},
    character::complete::{i64 as parse_i64, not_line_ending, space0, space1, u64 as parse_u64},
};

use crate::cmd::RawCommandOutput;
use crate::types::Devid;

use super::ParseError;
use super::types::{BtrfsDeviceUsageEntry, BtrfsDeviceUsageOutput, DeviceAllocation};

/// Kernel `btrfs_dev_name()` marker for a BTRFS_DEV_STATE_MISSING device, copied through
/// BTRFS_IOC_DEV_INFO by btrfs device usage (reference/btrfs-progs/cmds/filesystem-usage.c).
pub const MISSING_DEVICE_PATH_MARKER: &str = "<missing disk>";
/// btrfs-progs fallback when the dev-info ioctl returns an empty path.
pub const MISSING_DEVICE_PATH_FALLBACK: &str = "missing";

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
        devid: Devid::new(dev.devid),
        device_size,
        device_slack,
        allocations: dev.allocations,
        unallocated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::read_stable_fixture as fixture;

    fn is_dm_or_mapper_path(s: &str) -> bool {
        s.starts_with("/dev/dm-") || s.starts_with("/dev/mapper/braid-")
    }

    // --- Contract tests (nixos-26.05 fixtures) ---

    #[test]
    fn device_usage_parses_nixos_26_05_2disk() {
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
        assert_eq!(out.devices[0].devid, Devid::new(1));
        assert!(
            is_dm_or_mapper_path(&out.devices[1].path),
            "devid 2 path should be dm or mapper, got: {}",
            out.devices[1].path
        );
        assert_eq!(out.devices[1].devid, Devid::new(2));
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
        assert_eq!(out.devices[0].devid, Devid::new(1));
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
    // Intent: parse the missing-device marker `btrfs device usage`
    //   emits -- the literal `<missing disk>` path token.
    // Why it exists: remove-missing relocation checks depend on `device_size
    //   == 0`, devid, allocations, and unallocated bytes surviving even when
    //   the path is the `<missing disk>` marker. Pins the exact byte layout
    //   (column widths, indentation) that the captured
    //   `btrfs-device-usage-missing.txt` golden only checks structurally.
    // Scenario: one live device plus one absent device, rendered as
    //   `<missing disk>, ID: 3`. The Linux kernel's `btrfs_dev_name()`
    //   returns `<missing disk>` for a device with BTRFS_DEV_STATE_MISSING
    //   set, delivered via BTRFS_IOC_DEV_INFO; btrfs-progs copies it and
    //   only falls back to the literal `missing` when the ioctl hands back
    //   an empty path. Confirmed by the captured golden in both lanes.
    fn device_usage_parses_missing_device_marker() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vda, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:                 0\n\
                     \x20  Data,RAID1:            67108864\n\
                     \x20  Unallocated:          469762048\n\n\
                     <missing disk>, ID: 3\n\
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
        assert_eq!(missing.path, MISSING_DEVICE_PATH_MARKER);
        assert_eq!(missing.devid, Devid::new(3));
        assert_eq!(missing.device_size, 0);
        assert_eq!(missing.device_slack, 0);
        assert_eq!(missing.unallocated, 1_234_567);
        assert_eq!(missing.allocations.len(), 1);
        assert_eq!(missing.allocations[0].alloc_type, "Data");
        assert_eq!(missing.allocations[0].profile, "RAID1");
        assert_eq!(missing.allocations[0].bytes, 67_108_864);
    }

    // Intent: unknown non-allocation keys are ignored while required fields
    //   and allocation rows still parse correctly.
    // Why it exists: btrfs-progs may add per-device summary keys, and an
    //   additive key must not break braid's typed view of device usage.
    // Scenario: an updated or overridden btrfs-progs emits a new `FutureField`
    //   between the required summary keys and an existing allocation row.
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
    // Intent: a negative `Unallocated` from an in-progress `btrfs device
    //   remove` clamps to 0 while device size, slack, and allocations
    //   round-trip.
    // Why it exists: `parse_kv_line` parses values as i64 and clamps with
    //   `.max(0) as u64` only because btrfs reports negative Unallocated
    //   mid-remove. The captured `btrfs-device-usage-removing.txt` fixture
    //   locks the live output shape, while this synthetic test isolates the
    //   signed-value clamp so a refactor back to `parse_u64` fails immediately
    //   without requiring a VM fixture round-trip.
    // Scenario: the transient state captured by the
    //   `device remove progress observed` subtest in
    //   `tests/progress-monitoring.py` -- a device shedding block groups
    //   reports its full size as slack and a negative Unallocated.
    fn device_usage_clamps_negative_unallocated() {
        let raw = RawCommandOutput {
            cmd: "btrfs device usage".into(),
            stdout: "/dev/mapper/braid-vdc, ID: 3\n\
                     \x20  Device size:          4278190080\n\
                     \x20  Device slack:         4278190080\n\
                     \x20  Data,RAID1:           1073741824\n\
                     \x20  Metadata,RAID1:        268435456\n\
                     \x20  System,RAID1:           33554432\n\
                     \x20  Unallocated:          -1375731712\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_device_usage(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        let dev = &out.devices[0];
        // Negative Unallocated clamps to 0.
        assert_eq!(dev.unallocated, 0);
        // Device size and slack survive; slack == size is the remove signature.
        assert_eq!(dev.device_size, 4_278_190_080);
        assert_eq!(dev.device_slack, dev.device_size);
        // Allocations are preserved -- block groups are still being relocated off.
        assert_eq!(dev.allocations.len(), 3);
        assert_eq!(dev.used_bytes(), 1_073_741_824 + 268_435_456 + 33_554_432);
    }

    #[test]
    fn device_usage_used_bytes_helper() {
        let entry = BtrfsDeviceUsageEntry {
            path: "/dev/mapper/test".into(),
            devid: Devid::new(1),
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
            devid: Devid::new(1),
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
