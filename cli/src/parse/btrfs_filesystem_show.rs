use nom::{
    bytes::complete::{tag, take_until},
    character::complete::{not_line_ending, space0, space1, u64 as parse_u64},
    IResult,
};

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

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

// Parses: "Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098"  →  uuid string
fn parse_uuid_line(input: &str) -> IResult<&str, &str> {
    let (input, _) = take_until("uuid: ")(input)?;
    let (input, _) = tag("uuid: ")(input)?;
    let (input, uuid) = not_line_ending(input)?;
    Ok((input, uuid.trim()))
}

// Parses: "\tTotal devices 3 FS bytes used 1.00GiB"  →  3
fn parse_total_devices(input: &str) -> IResult<&str, u64> {
    let (input, _) = take_until("Total devices")(input)?;
    let (input, _) = tag("Total devices")(input)?;
    let (input, _) = space1(input)?;
    let (input, count) = parse_u64(input)?;
    Ok((input, count))
}

// Parses: "\tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-vda"
//      → (1, "/dev/mapper/braid-vda")
fn parse_devid_line(input: &str) -> IResult<&str, (u64, &str)> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("devid")(input)?;
    let (input, _) = space1(input)?;
    let (input, devid) = parse_u64(input)?;
    let (input, _) = take_until("path ")(input)?;
    let (input, _) = tag("path")(input)?;
    let (input, _) = space1(input)?;
    let (input, path) = not_line_ending(input)?;
    Ok((input, (devid, path.trim())))
}

// Parses: "\t*** Some devices missing"
fn parse_missing_sentinel(input: &str) -> IResult<&str, ()> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("*** Some devices missing")(input)?;
    Ok((input, ()))
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

    let stdout = &raw.stdout;

    let total_devices = stdout
        .lines()
        .find_map(|line| parse_total_devices(line).ok().map(|(_, c)| c))
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "Total devices".into(),
        })?;

    // Filter out missing-device placeholders. btrfs-progs prints either:
    //   devid  2 size 0 used 0 path /dev/mapper/disk-2 MISSING
    //   devid  2 size 0 used 0 path MISSING
    // These are synthetic — the device is gone. Only real present devices
    // are included; non-mapper real paths (e.g. /dev/sda1) are kept so
    // probe_pool can hard-fail on the invariant violation.
    let devices: Vec<BtrfsShowDevice> = stdout
        .lines()
        .filter_map(|line| {
            parse_devid_line(line).ok().and_then(|(_, (devid, path))| {
                if path == "MISSING" || path.ends_with(" MISSING") {
                    None
                } else {
                    Some(BtrfsShowDevice {
                        devid,
                        path: path.to_owned(),
                    })
                }
            })
        })
        .collect();

    let has_missing = stdout
        .lines()
        .any(|line| parse_missing_sentinel(line).is_ok());

    let uuid = stdout
        .lines()
        .find_map(|line| parse_uuid_line(line).ok().map(|(_, u)| u.to_owned()));

    Ok(BtrfsFilesystemShowOutput {
        uuid,
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
            "{}/tests/fixtures/nixos-25.11/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn btrfs_show_parses_nixos_25_11_2disk() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-2disk.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 2);
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].path, "/dev/mapper/braid-vdb");
        assert_eq!(
            out.uuid.as_deref(),
            Some("cc86845b-aec3-408e-bef5-553affc1f2b1"),
            "FSID must be parsed from uuid line"
        );
        assert_eq!(out.devices[1].devid, 2);
        assert_eq!(out.devices[1].path, "/dev/mapper/braid-vdc");
        assert!(!out.has_missing);
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn btrfs_show_detects_degraded_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098\n\
                     \tTotal devices 2 FS bytes used 4.00GiB\n\
                     \tdevid    1 size 10.00GiB used 5.00GiB path /dev/mapper/braid-vda\n\
                     \t*** Some devices missing\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 1);
        assert!(out.has_missing);
    }

    /// btrfs-progs prints `path /dev/mapper/X MISSING` for gone devices.
    /// Parser excludes the placeholder; total_devices and has_missing are authoritative.
    #[test]
    fn btrfs_show_excludes_missing_sentinel_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098\n\
                     \tTotal devices 2 FS bytes used 4.00GiB\n\
                     \tdevid    1 size 10.00GiB used 5.00GiB path /dev/mapper/braid-vda\n\
                     \tdevid    2 size 0 used 0 path /dev/mapper/braid-vdb MISSING\n\
                     \t*** Some devices missing\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(
            out.devices.len(),
            1,
            "MISSING sentinel device must be excluded"
        );
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].path, "/dev/mapper/braid-vda");
        assert!(out.has_missing);
    }

    /// When a device is fully gone, btrfs-progs prints bare `path MISSING`
    /// without a /dev/mapper/ prefix. The parser must exclude these too.
    /// Bug: `ends_with(" MISSING")` didn't catch bare `MISSING`.
    #[test]
    fn btrfs_show_excludes_bare_missing_path() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098\n\
                     \tTotal devices 2 FS bytes used 4.00GiB\n\
                     \tdevid    1 size 10.00GiB used 5.00GiB path /dev/mapper/braid-vda\n\
                     \tdevid    2 size 0 used 0 path MISSING\n\
                     \t*** Some devices missing\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 1, "bare MISSING path must be excluded");
        assert_eq!(out.devices[0].devid, 1);
        assert!(out.has_missing);
    }

    #[test]
    fn btrfs_show_parses_fsid_from_uuid_line() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: f1e2d3c4-b5a6-9788-7654-321fedcba098\n\
                     \tTotal devices 1 FS bytes used 4.00GiB\n\
                     \tdevid    1 size 10.00GiB used 5.00GiB path /dev/mapper/braid-vda\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(
            out.uuid.as_deref(),
            Some("f1e2d3c4-b5a6-9788-7654-321fedcba098")
        );
    }

    #[test]
    fn btrfs_show_rejects_malformed_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "This is not btrfs output at all\nrandom garbage data".into(),
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
        assert!(matches!(
            classify_btrfs_probe(&raw),
            DeviceBtrfsProbe::HasBtrfs
        ));
    }

    #[test]
    fn classify_btrfs_probe_not_valid_filesystem() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: not a valid btrfs filesystem on /dev/dm-0".into(),
            exit_status: 1,
        };
        assert!(matches!(
            classify_btrfs_probe(&raw),
            DeviceBtrfsProbe::NoBtrfs
        ));
    }

    #[test]
    fn classify_btrfs_probe_no_btrfs() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: no btrfs on /dev/dm-0".into(),
            exit_status: 1,
        };
        assert!(matches!(
            classify_btrfs_probe(&raw),
            DeviceBtrfsProbe::NoBtrfs
        ));
    }

    #[test]
    fn classify_btrfs_probe_unknown_error() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: String::new(),
            stderr: "ERROR: unexpected internal error".into(),
            exit_status: 1,
        };
        assert!(matches!(
            classify_btrfs_probe(&raw),
            DeviceBtrfsProbe::Unknown(_)
        ));
    }
}
