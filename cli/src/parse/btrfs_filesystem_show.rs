use nom::{
    IResult,
    bytes::complete::{tag, take_until},
    character::complete::{not_line_ending, space0, space1, u64 as parse_u64},
};

use crate::cmd::RawCommandOutput;
use crate::types::{Devid, Fsid};

use super::ParseError;
use super::types::{BtrfsFilesystemShowOutput, BtrfsShowDevice};

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

    // Separate present devices from missing-device placeholders.
    // btrfs-progs prints either:
    //   devid  2 size 0 used 0 path /dev/mapper/disk-2 MISSING
    //   devid  2 size 0 used 0 path MISSING
    // These are synthetic — the device is gone. Only real present devices
    // are included; non-mapper real paths (e.g. /dev/sda1) are kept so
    // probe_pool can hard-fail on the invariant violation.
    let mut devices: Vec<BtrfsShowDevice> = Vec::new();
    let mut missing_devids: Vec<Devid> = Vec::new();
    for line in stdout.lines() {
        if let Ok((_, (devid, path))) = parse_devid_line(line) {
            if path == "MISSING" || path.ends_with(" MISSING") {
                missing_devids.push(Devid::new(devid));
            } else {
                devices.push(BtrfsShowDevice {
                    devid: Devid::new(devid),
                    path: path.to_owned(),
                });
            }
        }
    }

    let has_missing = stdout
        .lines()
        .any(|line| parse_missing_sentinel(line).is_ok());

    // Route the found FSID through Fsid::parse so the value-type is the single
    // source of canonicalization, mirroring how parse_cryptsetup_luks_uuid_from_dump
    // builds a LuksUuid. An absent uuid line stays None; a present-but-malformed
    // FSID becomes ParseError::InvalidValue rather than a silently-untyped string.
    let uuid = stdout
        .lines()
        .find_map(|line| parse_uuid_line(line).ok().map(|(_, u)| u))
        .map(|found| {
            Fsid::parse(found).map_err(|e| ParseError::InvalidValue {
                cmd: raw.cmd.clone(),
                field: "uuid".into(),
                raw: e.raw,
                detail: e.detail,
            })
        })
        .transpose()?;

    Ok(BtrfsFilesystemShowOutput {
        uuid,
        total_devices,
        devices,
        has_missing,
        missing_devids,
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
    fn btrfs_show_parses_nixos_26_05_2disk() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: fixture("btrfs-show-2disk.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_filesystem_show(&raw).unwrap();
        assert_eq!(out.total_devices, 2);
        assert_eq!(out.devices.len(), 2);
        assert_eq!(out.devices[0].devid, Devid::new(1));
        assert!(
            is_dm_or_mapper_path(&out.devices[0].path),
            "devid 1 path should be dm or mapper, got: {}",
            out.devices[0].path
        );
        let uuid = out
            .uuid
            .as_ref()
            .expect("FSID must be parsed from uuid line");
        assert!(
            uuid::Uuid::parse_str(uuid.as_str()).is_ok(),
            "FSID should be a valid UUID, got: {uuid}"
        );
        assert_eq!(out.devices[1].devid, Devid::new(2));
        assert!(
            is_dm_or_mapper_path(&out.devices[1].path),
            "devid 2 path should be dm or mapper, got: {}",
            out.devices[1].path
        );
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
        assert_eq!(out.devices[0].devid, Devid::new(1));
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
        assert_eq!(out.devices[0].devid, Devid::new(1));
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
            out.uuid.as_ref().map(Fsid::as_str),
            Some("f1e2d3c4-b5a6-9788-7654-321fedcba098")
        );
    }

    // Intent: a present-but-malformed `uuid:` value surfaces as
    //   ParseError::InvalidValue naming the `uuid` field, parallel to the
    //   cryptsetup luks_uuid_from_dump_returns_invalid_value_when_unparseable
    //   test.
    // Why it exists: the FSID value-type is constructed here via Fsid::parse;
    //   an upstream btrfs-progs that emitted a non-UUID FSID must fail loudly
    //   at the producer rather than flow an untyped string into the
    //   plan->recover identity comparison.
    // Scenario: a corrupted or future-format btrfs filesystem show prints a
    //   `uuid:` line whose value is not a UUID.
    #[test]
    fn btrfs_show_returns_invalid_value_for_malformed_fsid() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem show".into(),
            stdout: "Label: none  uuid: not-a-uuid\n\
                     \tTotal devices 1 FS bytes used 4.00GiB\n\
                     \tdevid    1 size 10.00GiB used 5.00GiB path /dev/mapper/braid-vda\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_filesystem_show(&raw).unwrap_err();
        match err {
            ParseError::InvalidValue {
                field, raw, detail, ..
            } => {
                assert_eq!(field, "uuid");
                assert_eq!(raw, "not-a-uuid");
                assert!(!detail.is_empty(), "detail must carry uuid-crate reason");
            }
            other => panic!("expected InvalidValue uuid, got {other:?}"),
        }
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
