use std::collections::BTreeSet;

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::mapper_name;
use crate::dm_identity::mapper_luks_uuid;
use crate::filesystem::Filesystem;
use crate::luks::{
    BackingPathResolver, MapperOwnership, MapperOwnershipFailure, OwnershipError,
    classify_mapper_ownership,
};
use crate::parse::types::{BackingDevice, CryptsetupStatusOutput};
use crate::parse::{
    ParseError, parse_btrfs_filesystem_show, parse_cryptsetup_luks_label,
    parse_cryptsetup_luks_uuid, parse_cryptsetup_luks_version, parse_cryptsetup_status,
};
use crate::types::{Devid, *};

// ---------------------------------------------------------------------------
// ProbeError
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("pool device {mapper}: {detail}")]
    PoolDevice { mapper: String, detail: String },
    #[error("{mount_point} is mounted but fstype is {fstype}, not btrfs")]
    NotBtrfs { mount_point: String, fstype: String },
    #[error(
        "disk '{name}' is LUKS{version}; braid requires LUKS2. \
         To use this disk with braid, back up its data and re-add it \
         (braid will reformat it as LUKS2)."
    )]
    UnsupportedLuksVersion { name: String, version: u32 },
    #[error("{0}")]
    MapperOwnership(#[from] MapperOwnershipFailure),
    #[error("mountinfo error: {0}")]
    MountInfo(#[from] crate::mount_check::MountInfoError),
}

impl From<OwnershipError> for ProbeError {
    fn from(err: OwnershipError) -> Self {
        match err {
            OwnershipError::MapperOwnership(err) => ProbeError::MapperOwnership(err),
            OwnershipError::Parse(err) => ProbeError::Parse(err),
            OwnershipError::Cmd(err) => ProbeError::Cmd(err),
        }
    }
}

// ---------------------------------------------------------------------------
// probe_config_disk
// ---------------------------------------------------------------------------

pub fn probe_config_disk<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &DiskName,
    by_id: &ByIdPath,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<ConfigDisk, ProbeError> {
    if !fs.exists(by_id.as_str()) {
        return Ok(ConfigDisk {
            name: name.clone(),
            by_id_path: by_id.clone(),
            state: ConfigDiskState::Absent,
        });
    }

    let raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: by_id.as_str().to_owned(),
    })?;

    let uuid = match parse_cryptsetup_luks_uuid(&raw) {
        Ok(out) => out.uuid,
        Err(ParseError::CommandFailed { .. }) => {
            return Ok(ConfigDisk {
                name: name.clone(),
                by_id_path: by_id.clone(),
                state: ConfigDiskState::PresentNotLuks,
            });
        }
        Err(e) => return Err(ProbeError::Parse(e)),
    };

    // Enforce braid's LUKS2-only invariant at the gateway. The luksUUID
    // call above accepts both LUKS1 and LUKS2 (it passes no --type), so we
    // ask luksDump for the version explicitly.
    //
    // Any failure from here propagates as a hard error rather than falling
    // through to PresentLuks: the gateway must not report an unconfirmed disk
    // as a healthy PresentLuks member. This is NOT how genuine LUKS2 metadata
    // damage surfaces, though. luksUUID and luksDump both gate on the same
    // cryptsetup crypt_load (neither passes --type), so real damage fails the
    // luksUUID above first -> PresentNotLuks, which plan_open_pool routes into
    // the degraded-refusable `missing` vector (so --allow-degraded works).
    // The complete set of causes that can still reach here after luksUUID
    // succeeded, all correctly fail-closed:
    //   1. luksDump fails to spawn -> ProbeError::Cmd (environmental).
    //   2. luksDump runs but exits non-zero -- a transient I/O/OOM fault on
    //      this second invocation, or a concurrent header rewrite between the
    //      two calls -- parsed as CommandFailed -> ProbeError::Parse.
    //   3. luksDump exits 0 but the `Version:` line is unparseable: cryptsetup
    //      output drift, a parser-compat signal that must stay loud ->
    //      ProbeError::Parse.
    //   4. A LUKS1 device (`Version: 1`) -> UnsupportedLuksVersion.
    // See probe_luks_header for the shared-crypt_load model.
    let dump_raw = runner.run(&CmdRequest::CryptsetupLuksDumpText {
        device: by_id.as_str().to_owned(),
    })?;
    let version = parse_cryptsetup_luks_version(&dump_raw)?.version;
    if version != 2 {
        return Err(ProbeError::UnsupportedLuksVersion {
            name: name.as_str().to_owned(),
            version,
        });
    }
    let label = parse_cryptsetup_luks_label(&dump_raw)?.label;

    let mn = mapper_name(name);
    let mapper_open =
        probe_mapper_open(runner, fs, name, &mn, by_id, backing_path_resolver, &uuid)?;

    Ok(ConfigDisk {
        name: name.clone(),
        by_id_path: by_id.clone(),
        state: ConfigDiskState::PresentLuks {
            uuid,
            label,
            mapper_open,
        },
    })
}

/// Determine whether `/dev/mapper/<mapper>` is open AND backed by the
/// configured disk's LUKS container. `cryptsetup status` decides activity:
/// a closed mapper is reported inactive, and an active mapper carries the
/// underlying device that we cross-check against the configured disk before
/// comparing the LUKS UUID the mapper records.
///
/// Returns `Ok(true)` when the mapper is open on the configured disk and
/// records `expected_uuid`. Returns `Ok(false)` when the mapper is inactive.
/// Returns `ProbeError::MapperOwnership` when the mapper is active but its
/// backing is missing (stale dm-crypt) or it records a different LUKS UUID
/// (external mapper aliasing over our name).
fn probe_mapper_open<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &DiskName,
    mapper: &MapperName,
    by_id: &ByIdPath,
    backing_path_resolver: &dyn BackingPathResolver,
    expected_uuid: &LuksUuid,
) -> Result<bool, ProbeError> {
    match classify_mapper_ownership(
        runner,
        fs,
        name,
        mapper,
        by_id,
        backing_path_resolver,
        || Ok(expected_uuid.clone()),
    )? {
        MapperOwnership::Inactive => Ok(false),
        MapperOwnership::Owned => Ok(true),
    }
}

// ---------------------------------------------------------------------------
// probe_pool
// ---------------------------------------------------------------------------

/// Alert-pipeline pool view. Ack and monitor need mount state, live devids,
/// btrfs-MISSING devids, null-underlying devids, and the optional pool FSID,
/// but not per-device LUKS identity or device counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertPoolState {
    pub mounted: bool,
    pub present_devids: Vec<Devid>,
    pub missing_devids: Vec<Devid>,
    /// Devids of open mappers whose backing disk is gone.
    pub null_underlying_devids: Vec<Devid>,
    /// Canonical btrfs filesystem UUID, the strongest field of the ENOSPC baseline's
    /// `PoolKey`. `None` only when the probe could not extract it; the monitor
    /// and ack then build no usable `PoolKey` rather than weaken the identity
    /// guard.
    pub fsid: Option<Fsid>,
}

/// Named carrier for the two devid sets `compute_alert_state` requires.
/// Grouping the two sets into one named struct
/// makes the positional `recognized`/`missing` swap an unwritable mistake --
/// a caller cannot pass `missing` where `recognized` is expected.
pub struct AlertDevids {
    /// All devids the alert pipeline recognizes as pool members (present +
    /// null-underlying + btrfs-MISSING). Stats rows outside this set are stale
    /// identities and are skipped. The `BTreeSet` type carries the dedup/order
    /// invariant so consumers can membership-test without rebuilding a set.
    pub recognized: BTreeSet<Devid>,
    /// Devids that must fire `MissingDevice` alerts: btrfs-MISSING union
    /// null-underlying. These skip `BtrfsDeviceErrors` because they are already
    /// alerted as missing. The `BTreeSet` type carries the dedup/order
    /// invariant.
    pub missing: BTreeSet<Devid>,
}

impl AlertPoolState {
    /// Build the named `AlertDevids` carrier for one probe snapshot.
    /// The two fields have distinct semantics; grouping them prevents a
    /// positional swap at every call site.
    pub fn alert_devids(&self) -> AlertDevids {
        let recognized = self
            .present_devids
            .iter()
            .copied()
            .chain(self.null_underlying_devids.iter().copied())
            .chain(self.missing_devids.iter().copied())
            .collect::<BTreeSet<Devid>>();
        let missing = self
            .missing_devids
            .iter()
            .copied()
            .chain(self.null_underlying_devids.iter().copied())
            .collect::<BTreeSet<Devid>>();
        AlertDevids {
            recognized,
            missing,
        }
    }
}

/// Narrowed alert-pipeline probe for `cmd_ack` and `cmd_monitor`. Preserves
/// `probe_pool`'s mount-state, btrfs-MISSING, and null-underlying detection
/// while omitting per-device LUKS UUID lookup. Unlike `probe_pool`, it tolerates
/// a missing FSID so device-alert paths can still run while ENOSPC keying falls
/// back to "no usable `PoolKey`".
pub fn probe_pool_alerts<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<AlertPoolState, ProbeError> {
    match crate::mount_check::fstype_at_mount_via_fs(fs, mount_point.as_str())? {
        None => {
            return Ok(AlertPoolState {
                mounted: false,
                present_devids: vec![],
                missing_devids: vec![],
                null_underlying_devids: vec![],
                fsid: None,
            });
        }
        Some(fstype) if fstype != "btrfs" => {
            return Err(ProbeError::NotBtrfs {
                mount_point: mount_point.as_str().to_owned(),
                fstype,
            });
        }
        Some(_) => {}
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.clone(),
    })?;
    let show = parse_btrfs_filesystem_show(&show_raw)?;
    let fsid = show.uuid;

    let mut present_devids = Vec::new();
    let mut null_underlying_devids = Vec::new();
    for bdev in &show.devices {
        let path = &bdev.path;

        if !path.starts_with("/dev/mapper/") {
            return Err(ProbeError::PoolDevice {
                mapper: path.clone(),
                detail: "not a /dev/mapper/ path".to_owned(),
            });
        }

        let name = path
            .strip_prefix("/dev/mapper/")
            .expect("checked above")
            .to_owned();

        let status_raw = runner.run(&CmdRequest::CryptsetupStatus {
            mapper: MapperName::from_basename(name.clone()),
        })?;
        match parse_cryptsetup_status(&status_raw)? {
            CryptsetupStatusOutput::Inactive => {
                return Err(ProbeError::PoolDevice {
                    mapper: name,
                    detail: "not active".to_owned(),
                });
            }
            CryptsetupStatusOutput::Active {
                backing: BackingDevice::Null,
            } => null_underlying_devids.push(bdev.devid),
            CryptsetupStatusOutput::Active {
                backing: BackingDevice::Path(_),
            } => present_devids.push(bdev.devid),
        }
    }

    Ok(AlertPoolState {
        mounted: true,
        present_devids,
        missing_devids: show.missing_devids,
        null_underlying_devids,
        fsid,
    })
}

/// Live view of the mounted pool: each btrfs device's mapper, the LUKS UUID
/// that mapper records, and whether its backing disk is still present. The
/// UUID comes from the mapper, not the backing disk, so a gone or unreadable
/// member keeps its identity.
pub fn probe_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<PoolState, ProbeError> {
    match crate::mount_check::fstype_at_mount_via_fs(fs, mount_point.as_str())? {
        None => {
            return Ok(PoolState {
                mounted: false,
                devices: vec![],
                missing_count: 0,
                total_devices: 0,
                fsid: None,
                missing_devids: vec![],
                null_underlying: vec![],
            });
        }
        Some(fstype) if fstype != "btrfs" => {
            return Err(ProbeError::NotBtrfs {
                mount_point: mount_point.as_str().to_owned(),
                fstype,
            });
        }
        Some(_) => {}
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.clone(),
    })?;
    let show = parse_btrfs_filesystem_show(&show_raw)?;

    // A mounted btrfs filesystem always has an FSID. None here means the
    // parser couldn't extract the uuid line — a broken invariant, not a
    // state we should silently propagate to consumers.
    let fsid = show.uuid.ok_or_else(|| ProbeError::PoolDevice {
        mapper: mount_point.as_str().to_owned(),
        detail: "mounted pool has no FSID in btrfs filesystem show output".into(),
    })?;

    let mut devices = Vec::new();
    let mut null_underlying = Vec::new();
    for bdev in &show.devices {
        let path = &bdev.path;

        if !path.starts_with("/dev/mapper/") {
            return Err(ProbeError::PoolDevice {
                mapper: path.clone(),
                detail: "not a /dev/mapper/ path".to_owned(),
            });
        }

        let name = path
            .strip_prefix("/dev/mapper/")
            .expect("checked above")
            .to_owned();

        let mapper = MapperName::from_basename(name);
        let status_raw = runner.run(&CmdRequest::CryptsetupStatus {
            mapper: mapper.clone(),
        })?;
        // When a backing device is hot-unplugged, cryptsetup reports
        // device: (null). Record these as null-underlying — the mapper
        // is open but the block device is gone. Both kinds carry the LUKS
        // UUID the mapper itself records, which needs no backing device.
        let backing = match parse_cryptsetup_status(&status_raw)? {
            CryptsetupStatusOutput::Inactive => {
                return Err(ProbeError::PoolDevice {
                    mapper: mapper.as_str().to_owned(),
                    detail: "not active".to_owned(),
                });
            }
            CryptsetupStatusOutput::Active { backing } => backing,
        };
        let luks_uuid = pool_device_luks_uuid(fs, &mapper)?;
        match backing {
            BackingDevice::Null => null_underlying.push(NullUnderlyingDevice {
                mapper,
                luks_uuid,
                devid: bdev.devid,
            }),
            BackingDevice::Path(underlying) => devices.push(PoolDevice {
                mapper,
                luks_uuid,
                devid: bdev.devid,
                underlying: underlying.as_str().to_owned(),
            }),
        }
    }

    let missing_count = show.total_devices.saturating_sub(devices.len() as u64);

    Ok(PoolState {
        mounted: true,
        devices,
        missing_count,
        total_devices: show.total_devices,
        fsid: Some(fsid),
        missing_devids: show.missing_devids,
        null_underlying,
    })
}

/// The LUKS UUID a pool device's mapper records. A pool device without a
/// provable one fails the probe, because every consumer correlates pool
/// devices with membership by UUID.
fn pool_device_luks_uuid<F: Filesystem + ?Sized>(
    fs: &F,
    mapper: &MapperName,
) -> Result<LuksUuid, ProbeError> {
    mapper_luks_uuid(fs, mapper).map_err(|unproven| ProbeError::PoolDevice {
        mapper: mapper.as_str().to_owned(),
        detail: unproven.to_string(),
    })
}

// ---------------------------------------------------------------------------
// probe_fsid
// ---------------------------------------------------------------------------

/// Resolve a mounted pool's FSID for preflight checks against
/// `/sys/fs/btrfs/<fsid>/exclusive_operation`, without probing
/// per-device cryptsetup state.
///
/// Preserves the `NotBtrfs` contract from `probe_pool`: if the mount
/// point is held by a non-btrfs filesystem, returns `ProbeError::NotBtrfs`
/// rather than a generic parse failure from `btrfs filesystem show`.
///
/// Caller must have already confirmed the mount point is active (e.g.
/// via `CmdRequest::MountpointCheck`).
pub fn probe_fsid<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<Fsid, ProbeError> {
    match crate::mount_check::fstype_at_mount_via_fs(fs, mount_point.as_str())? {
        None => {
            return Err(ProbeError::PoolDevice {
                mapper: mount_point.as_str().to_owned(),
                detail: "mount point not present in mountinfo".into(),
            });
        }
        Some(fstype) if fstype != "btrfs" => {
            return Err(ProbeError::NotBtrfs {
                mount_point: mount_point.as_str().to_owned(),
                fstype,
            });
        }
        Some(_) => {}
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.clone(),
    })?;
    let show = parse_btrfs_filesystem_show(&show_raw)?;
    show.uuid.ok_or_else(|| ProbeError::PoolDevice {
        mapper: mount_point.as_str().to_owned(),
        detail: "mounted pool has no FSID in btrfs filesystem show output".into(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::test_fixtures::{MockBackingPathResolver, MockDmTable};

    struct MockFs {
        paths: Vec<String>,
        block_devices: Vec<String>,
        mountinfo: Option<Result<String, std::io::ErrorKind>>,
        dm: MockDmTable,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
                mountinfo: None,
                dm: MockDmTable::default(),
            }
        }

        fn with_mountinfo(body: &str) -> Self {
            Self {
                paths: vec![],
                block_devices: vec![],
                mountinfo: Some(Ok(body.to_string())),
                dm: MockDmTable::default(),
            }
        }

        fn with_mountinfo_error(kind: std::io::ErrorKind) -> Self {
            Self {
                paths: vec![],
                block_devices: vec![],
                mountinfo: Some(Err(kind)),
                dm: MockDmTable::default(),
            }
        }

        /// Add an active LUKS2 mapping to the sysfs device-mapper table.
        fn with_luks_dm(mut self, mapper: &str, luks_uuid: &str) -> Self {
            self.dm = self.dm.with_luks(mapper, luks_uuid);
            self
        }

        /// Add an active device with an arbitrary dm UUID.
        fn with_raw_dm(mut self, mapper: &str, dm_uuid: &str) -> Self {
            self.dm = self.dm.with_raw(mapper, dm_uuid);
            self
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, path: &str) -> bool {
            self.block_devices.contains(&path.to_string())
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                return match &self.mountinfo {
                    Some(Ok(body)) => Ok(body.clone()),
                    Some(Err(kind)) => Err(std::io::Error::new(*kind, "mock mountinfo error")),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "mock mountinfo not seeded",
                    )),
                };
            }
            self.dm
                .read_to_string(path)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(self.dm.list_dir(path).unwrap_or_default())
        }

        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            unreachable!("probe::MockFs: read-only fixture; create_dir_all must never be called")
        }
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn by_id(path: &str) -> ByIdPath {
        ByIdPath::parse(path).unwrap()
    }

    fn dn(name: &str) -> DiskName {
        DiskName::parse(name).expect("valid disk name in test fixture")
    }

    fn mp() -> MountPoint {
        MountPoint::new("/mnt/storage".into())
    }

    // -- probe_config_disk tests --

    #[test]
    fn probe_config_disk_absent() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap();
        assert_eq!(result.name.as_str(), "toshiba");
        assert_eq!(result.state, ConfigDiskState::Absent);
    }

    #[test]
    fn probe_config_disk_present_not_luks() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk-1".into(),
            },
            err_raw(
                "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                4,
                "Device is not a valid LUKS device.",
            ),
        );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap();
        assert_eq!(result.state, ConfigDiskState::PresentNotLuks);
    }

    #[test]
    fn probe_config_disk_cmd_spawn_fails() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::Cmd(_)),
            "expected ProbeError::Cmd, got: {err:?}"
        );
    }

    #[test]
    fn probe_config_disk_garbled_uuid_output() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk-1".into(),
            },
            ok_raw("cryptsetup luksUUID /dev/disk/by-id/disk-1", "not-a-uuid\n"),
        );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::Parse(_)),
            "expected ProbeError::Parse, got: {err:?}"
        );
    }

    fn luks_dump_text_luks2() -> RawCommandOutput {
        ok_raw(
            "cryptsetup luksDump",
            "LUKS header information\n\
             Version:       \t2\n\
             UUID:          \ta1b2c3d4-e5f6-7890-abcd-ef1234567890\n\
             Label:         \tbraid-toshiba\n",
        )
    }

    fn luks_dump_text_luks1() -> RawCommandOutput {
        ok_raw(
            "cryptsetup luksDump",
            "LUKS header information\n\
             Version:       \t1\n\
             Cipher name:   \taes\n\
             Cipher mode:   \txts-plain64\n",
        )
    }

    fn cryptsetup_status_inactive(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
        }
    }

    fn cryptsetup_status_active_null(mapper: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tdevice:  (null)\n"
            ),
        )
    }

    #[test]
    fn probe_config_disk_present_luks_closed() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_inactive("braid-toshiba"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap();
        assert_eq!(result.name.as_str(), "toshiba");
        assert_eq!(
            result.state,
            ConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
                label: Some("braid-toshiba".to_owned()),
                mapper_open: false,
            }
        );
    }

    /*
     * Intent: when the mapper is open AND cryptsetup status reports a
     *   backing device whose LUKS UUID matches the configured disk, the
     *   probe must report mapper_open=true.
     * Why it exists: regression guard that the source-of-truth shift from
     *   fs.exists to cryptsetup status still admits the healthy
     *   already-open case end-to-end (status active + backing UUID
     *   match -> mapper_open=true, no error).
     * Scenario: braid status / unlock run after a successful prior
     *   unlock; the mapper is open and backed by the correct disk.
     */
    #[test]
    fn probe_config_disk_present_luks_open() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"])
            .with_luks_dm("braid-toshiba", "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        let d = by_id("/dev/disk/by-id/disk-1");

        let resolver =
            MockBackingPathResolver::default().with_path("/dev/disk/by-id/disk-1", "/dev/vda");
        let result = probe_config_disk(&runner, &fs, &dn("toshiba"), &d, &resolver).unwrap();
        assert_eq!(
            result.state,
            ConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
                label: Some("braid-toshiba".to_owned()),
                mapper_open: true,
            }
        );
    }

    /*
     * Intent: when /dev/mapper/braid-<name> is open but backed by a LUKS
     *   container with a different UUID than the configured disk, the
     *   probe must surface ProbeError::MapperOwnership(Conflict) instead of
     *   reporting mapper_open=true.
     * Why it exists: this is the failure-layer test for the
     *   path-existence regression in `probe_mapper_open`.
     *   Reverting probe_mapper_open back to fs.exists makes this test
     *   fail (mapper_open would become true), per
     *   feedback_test_at_failure_layer.md. Parser canaries cannot catch
     *   this wiring bug.
     * Scenario: a user or systemd-cryptsetup has opened an unrelated
     *   LUKS container under the name braid-toshiba before running
     *   braid unlock.
     */
    #[test]
    fn probe_config_disk_mapper_backing_mismatch_errors() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vdz"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"])
            .with_luks_dm("braid-toshiba", "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        let d = by_id("/dev/disk/by-id/disk-1");

        let err = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap_err();
        match err {
            ProbeError::MapperOwnership(MapperOwnershipFailure::BackingPathMismatch {
                name,
                expected_path,
                found_path,
            }) => {
                assert_eq!(name, "toshiba");
                assert_eq!(expected_path, "/dev/disk/by-id/disk-1");
                assert_eq!(found_path, "/dev/vdz");
            }
            other => panic!("expected mapper backing-path mismatch, got: {other:?}"),
        }
        let by_id_luks_uuid_requests = runner
            .requests()
            .iter()
            .filter(|request| {
                matches!(
                    request,
                    CmdRequest::CryptsetupLuksUuid { device }
                        if device.as_str() == "/dev/disk/by-id/disk-1"
                )
            })
            .count();
        assert_eq!(
            by_id_luks_uuid_requests, 1,
            "planner must not refetch the configured by-id LUKS UUID during mapper ownership check"
        );
    }

    /*
     * Intent: probe_config_disk preserves backing-path resolve failures as a
     *   dedicated ProbeError variant with udev/device remediation wording.
     * Why it exists: probe callers must not see these failures as UUID
     *   conflicts or generic parse errors.
     * Scenario: the configured by-id path vanishes between the initial
     *   existence check and the already-open mapper ownership check.
     */
    #[test]
    fn probe_config_disk_mapper_backing_resolve_error_is_distinct() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");
        let resolver = MockBackingPathResolver::default()
            .with_error("/dev/disk/by-id/disk-1", std::io::ErrorKind::NotFound);

        let err = probe_config_disk(&runner, &fs, &dn("toshiba"), &d, &resolver).unwrap_err();
        let msg = err.to_string();
        match &err {
            ProbeError::MapperOwnership(MapperOwnershipFailure::BackingPathResolveError {
                name,
                by_id,
                source,
            }) => {
                assert_eq!(name, "toshiba");
                assert_eq!(by_id, "/dev/disk/by-id/disk-1");
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected mapper backing-path resolve error, got {other:?}"),
        }
        assert!(
            msg.contains("Check that the configured disk is plugged in"),
            "resolver remediation missing from Display: {msg}"
        );
    }

    // Intent: when an active mapper's dm UUID is not a cryptsetup LUKS one,
    //   the probe surfaces ProbeError::MapperOwnership(Conflict) with
    //   found=None.
    // Why it exists: mapper ownership failure should use the same recovery
    //   path as executor-time checks, not a generic parse error.
    // Scenario: a plain dm-crypt mapping has taken braid's mapper name.
    #[test]
    fn probe_config_disk_mapper_backing_non_luks_errors() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vdz"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"])
            .with_raw_dm("braid-toshiba", "CRYPT-PLAIN-braid-toshiba");
        let d = by_id("/dev/disk/by-id/disk-1");

        let resolver =
            MockBackingPathResolver::default().with_path("/dev/disk/by-id/disk-1", "/dev/vdz");
        let err = probe_config_disk(&runner, &fs, &dn("toshiba"), &d, &resolver).unwrap_err();

        match err {
            ProbeError::MapperOwnership(MapperOwnershipFailure::Conflict {
                name,
                expected,
                found,
            }) => {
                assert_eq!(name, "toshiba");
                assert_eq!(
                    expected,
                    LuksUuid::parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap()
                );
                assert_eq!(found, None);
            }
            other => panic!("expected mapper conflict, got: {other:?}"),
        }
    }

    /*
     * Intent: when cryptsetup status reports the mapper as inactive, the
     *   probe must report mapper_open=false without error so the normal
     *   unlock flow opens the LUKS container fresh.
     * Why it exists: cryptsetup status is the sole source of truth for
     *   mapper state; the probe must handle the inactive case as "not
     *   open", not as an error. Guards against a future refactor that
     *   might treat any cryptsetup-status non-zero exit as a hard
     *   failure.
     * Scenario: a fresh boot where no mapper has been opened yet; also
     *   the TOCTOU race where the mapper was torn down between any
     *   prior observation and the status query.
     */
    #[test]
    fn probe_config_disk_mapper_status_inactive_is_closed() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_inactive("braid-toshiba"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap();
        assert_eq!(
            result.state,
            ConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
                label: Some("braid-toshiba".to_owned()),
                mapper_open: false,
            }
        );
    }

    /*
     * Intent: when cryptsetup status reports the mapper as active but
     *   with device = (null), the probe must surface
     *   ProbeError::MapperOwnership(Conflict) with found=None so downstream
     *   mutations do not operate on a stale mapper whose backing disk
     *   is gone.
     * Why it exists: hot-unplug leaves the mapper structure present but
     *   unusable; mount/add/replace reading from this mapper would see
     *   a detached block device. Fail-closed per
     *   feedback_fail_closed_by_downstream_blast_radius.md.
     * Scenario: user hot-unplugs the backing disk during active use;
     *   the kernel detaches the block device but dm-crypt has not yet
     *   torn down the mapper.
     */
    #[test]
    fn probe_config_disk_mapper_backing_null_errors() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks2(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active_null("braid-toshiba"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let err = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap_err();
        match err {
            ProbeError::MapperOwnership(MapperOwnershipFailure::Conflict {
                name,
                expected,
                found,
            }) => {
                assert_eq!(name, "toshiba");
                assert_eq!(
                    expected,
                    LuksUuid::parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap()
                );
                assert_eq!(found, None);
            }
            other => panic!("expected mapper conflict, got: {other:?}"),
        }
    }

    /*
     * Intent: a LUKS1-formatted disk that braid is configured to use must
     *   surface as ProbeError::UnsupportedLuksVersion at the gateway, not
     *   silently flow through to mutating commands.
     * Why it exists: this is the primary failure-layer test for the
     *   "braid only supports LUKS2" invariant. Per
     *   feedback_test_at_failure_layer.md, the bug fix's primary test
     *   must FAIL when the bug is reintroduced — re-adding `--type luks2`
     *   to the header probe alone would not be caught by this test, but
     *   removing the version check from probe_config_disk would.
     * Scenario: a user externally formats a disk as LUKS1 and tries to
     *   use it via any braid command (status, mount, add, etc.).
     */
    #[test]
    fn probe_config_disk_luks1_returns_unsupported_version() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                luks_dump_text_luks1(),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let err = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap_err();
        match err {
            ProbeError::UnsupportedLuksVersion { name, version } => {
                assert_eq!(name, "toshiba");
                assert_eq!(version, 1);
            }
            other => panic!("expected UnsupportedLuksVersion, got: {other:?}"),
        }
    }

    /*
     * Intent: if luksDump itself fails to spawn after luksUuid succeeds,
     *   the error must propagate as ProbeError::Cmd, not silently coerce
     *   into a "valid LUKS2" branch.
     * Why it exists: defensive against the version-check sneaking past
     *   real cryptsetup failures (e.g., binary missing, permission denied).
     * Scenario: cryptsetup binary disappears between the luksUuid and
     *   luksDump calls — pathological, but the gateway must surface it.
     */
    #[test]
    fn probe_config_disk_luksdump_failure_propagates_as_cmd_error() {
        // luksUuid mock present; luksDump intentionally absent → MissingMock.
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk-1".into(),
            },
            ok_raw(
                "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
            ),
        );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let err = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ProbeError::Cmd(_)),
            "expected ProbeError::Cmd, got: {err:?}"
        );
    }

    /*
     * Intent: if luksDump succeeds (exit 0) but the output has no
     *   parseable Version field, surface ProbeError::Parse.
     * Why it exists: a future cryptsetup output drift that drops the
     *   Version field must fail loudly at the probe layer rather than
     *   silently passing the gateway.
     * Scenario: hypothetical upstream output change.
     */
    #[test]
    fn probe_config_disk_luksdump_garbled_propagates_as_parse_error() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                    "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk-1".into(),
                },
                ok_raw(
                    "cryptsetup luksDump",
                    "LUKS header information\nUUID: foo\n",
                ),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let err = probe_config_disk(
            &runner,
            &fs,
            &dn("toshiba"),
            &d,
            &MockBackingPathResolver::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ProbeError::Parse(_)),
            "expected ProbeError::Parse, got: {err:?}"
        );
    }

    // -- probe_pool tests --

    fn mountinfo_without_target() -> String {
        "26 25 0:23 / / rw,noatime shared:1 - ext4 /dev/sda1 rw\n".to_string()
    }

    fn mountinfo_unrelated_target() -> String {
        "36 35 0:32 / /mnt/other rw,noatime shared:1 - ext4 /dev/sda1 rw\n".to_string()
    }

    fn mountinfo_btrfs() -> String {
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-toshiba rw\n"
            .to_string()
    }

    fn mountinfo_ext4() -> String {
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n".to_string()
    }

    fn malformed_mountinfo_for_target() -> String {
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 no_dash_separator\n".to_string()
    }

    fn duplicate_mountinfo_for_target() -> String {
        format!("{}{}", mountinfo_btrfs(), mountinfo_btrfs())
    }

    fn btrfs_show_2disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n",
        )
    }

    fn btrfs_show_3disk_1missing() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n\
             \t*** Some devices missing\n",
        )
    }

    fn cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tcipher:  aes-xts-plain64\n\
                 \tdevice:  {device}\n\
                 \tsector size:  512\n"
            ),
        )
    }

    #[test]
    fn probe_pool_unmounted() {
        let fs = MockFs::with_mountinfo(&mountinfo_without_target());
        let runner = MockRunner::default();

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(!result.mounted);
        assert!(result.devices.is_empty());
        assert_eq!(result.missing_count, 0);
    }

    #[test]
    fn probe_pool_unmounted_target_mismatch() {
        let fs = MockFs::with_mountinfo(&mountinfo_unrelated_target());
        let runner = MockRunner::default();

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(!result.mounted);
        assert!(result.devices.is_empty());
        assert_eq!(result.missing_count, 0);
    }

    #[test]
    fn probe_pool_mounted_not_btrfs() {
        let fs = MockFs::with_mountinfo(&mountinfo_ext4());
        let runner = MockRunner::default();

        let result = probe_pool(&runner, &fs, &mp());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::NotBtrfs { ref fstype, .. } if fstype == "ext4"),
            "expected ProbeError::NotBtrfs, got: {err:?}"
        );
    }

    #[test]
    fn probe_pool_mounted_2disk() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-ironwolf", "22222222-2222-2222-2222-222222222222");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_2disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            );

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 2);
        assert_eq!(result.missing_count, 0);
        assert_eq!(result.total_devices, 2);
        assert_eq!(
            result.devices[0].mapper,
            MapperName::from_basename("braid-toshiba".into())
        );
        assert_eq!(
            result.devices[0].luks_uuid,
            LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap()
        );
        assert_eq!(result.devices[0].devid, Devid::new(1));
        assert_eq!(
            result.devices[1].mapper,
            MapperName::from_basename("braid-ironwolf".into())
        );
        assert_eq!(
            result.fsid.as_ref().map(Fsid::as_str),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "pool FSID must be populated from btrfs filesystem show"
        );
    }

    #[test]
    fn probe_pool_mounted_with_missing() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-ironwolf", "22222222-2222-2222-2222-222222222222");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_3disk_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            );

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 2);
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 3);
    }

    #[test]
    fn probe_pool_degraded_missing_sentinel() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 0 used 0 path /dev/mapper/braid-ironwolf MISSING\n\
                     \t*** Some devices missing\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 1, "MISSING device must be excluded");
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 2);
        assert_eq!(
            result.devices[0].mapper,
            MapperName::from_basename("braid-toshiba".into())
        );
    }

    #[test]
    fn probe_pool_mapper_not_active() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                err_raw(
                    "cryptsetup status braid-toshiba",
                    4,
                    "/dev/mapper/braid-toshiba is not active.\n",
                ),
            );

        let result = probe_pool(&runner, &fs, &mp());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. } if detail == "not active"),
            "expected ProbeError::PoolDevice not active, got: {err:?}"
        );
    }

    #[test]
    fn probe_pool_non_mapper_device() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            ok_raw(
                "btrfs filesystem show /mnt/storage",
                "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/sda1\n",
            ),
        );

        let result = probe_pool(&runner, &fs, &mp());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref mapper, ref detail }
                if mapper == "/dev/sda1" && detail == "not a /dev/mapper/ path"),
            "expected ProbeError::PoolDevice non-mapper, got: {err:?}"
        );
    }

    // Intent: probe_pool records a mapper whose backing disk is gone as
    //   null-underlying, with the LUKS UUID its dm UUID records.
    // Why it exists: the dead member must keep its identity so lock, status,
    //   and recover bind it by UUID; reading the gone disk's header would
    //   fail (`cryptsetup luksUUID (null)`).
    // Scenario: braid-ironwolf's disk was hot-unplugged; btrfs still lists
    //   the mapper path and cryptsetup status reports `device: (null)`.
    #[test]
    fn probe_pool_device_null_underlying() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-ironwolf", "22222222-2222-2222-2222-222222222222");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                // cryptsetup reports device: (null) when backing device vanishes
                ok_raw(
                    "cryptsetup status braid-ironwolf",
                    "/dev/mapper/braid-ironwolf is active and is in use.\n\
                     \ttype:    LUKS2\n\
                     \tcipher:  aes-xts-plain64\n\
                     \tdevice:  (null)\n\
                     \tsector size:  512\n",
                ),
            );

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert!(result.mounted);
        assert_eq!(
            result.devices.len(),
            1,
            "device with (null) underlying must be skipped"
        );
        assert_eq!(
            result.devices[0].mapper,
            MapperName::from_basename("braid-toshiba".into())
        );
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 2);

        // Null-underlying device is captured for alert path resolution
        assert_eq!(result.null_underlying.len(), 1);
        assert_eq!(
            result.null_underlying[0].mapper,
            MapperName::from_basename("braid-ironwolf".into())
        );
        assert_eq!(result.null_underlying[0].devid, Devid::new(2));
        assert_eq!(
            result.null_underlying[0].luks_uuid,
            LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
            "a null-backed mapper keeps the LUKS UUID its dm UUID records"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksUuid { .. })),
            "probe_pool must not reread any LUKS header"
        );

        // missing_devids stays btrfs-authoritative — null-underlying devids
        // are NOT injected (remove-missing uses this for destructive targets)
        assert!(result.missing_devids.is_empty());
    }

    // Intent: probe_pool fails closed when a pool device's mapper has no
    //   provable LUKS UUID in the device-mapper table.
    // Why it exists: every consumer correlates pool devices with membership
    //   by UUID; a device without one must not be dropped or guessed.
    // Scenario: btrfs lists braid-toshiba and cryptsetup reports it active,
    //   but its dm UUID is not a cryptsetup LUKS one.
    #[test]
    fn probe_pool_unprovable_dm_identity_fails_closed() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_raw_dm("braid-toshiba", "CRYPT-PLAIN-braid-toshiba");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );

        let err = probe_pool(&runner, &fs, &mp()).unwrap_err();

        match err {
            ProbeError::PoolDevice { mapper, detail } => {
                assert_eq!(mapper, "braid-toshiba");
                assert_eq!(
                    detail,
                    "device-mapper UUID \"CRYPT-PLAIN-braid-toshiba\" is not a cryptsetup LUKS UUID"
                );
            }
            other => panic!("expected ProbeError::PoolDevice, got: {other:?}"),
        }
    }

    #[test]
    fn probe_pool_missing_count_saturates() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs())
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 0 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );

        let result = probe_pool(&runner, &fs, &mp()).unwrap();
        assert_eq!(
            result.missing_count, 0,
            "saturating_sub should prevent underflow"
        );
    }

    // A mounted pool whose btrfs filesystem show output has no uuid line
    // is a broken invariant — probe_pool must reject it rather than
    // returning PoolState with fsid: None, which would let downstream
    // consumers silently skip FSID-based safety guards.
    #[test]
    fn probe_pool_errors_on_missing_fsid() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            ok_raw(
                "btrfs filesystem show /mnt/storage",
                "\tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
            ),
        );

        let result = probe_pool(&runner, &fs, &mp());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. }
                if detail.contains("no FSID")),
            "expected ProbeError::PoolDevice about missing FSID, got: {err:?}"
        );
    }

    /* Intent: probe_pool propagates an IO error while reading mountinfo
     *   instead of treating the pool as offline.
     * Why: regression guard for the old fail-open path where mount-probe
     *   uncertainty could be mistaken for an offline pool.
     * Scenario: mountinfo cannot be read.
     */
    #[test]
    fn probe_pool_propagates_mountinfo_io_error() {
        let fs = MockFs::with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let runner = MockRunner::default();

        let result = probe_pool(&runner, &fs, &mp());
        assert!(matches!(
            result,
            Err(ProbeError::MountInfo(
                crate::mount_check::MountInfoError::Io(_)
            ))
        ));
    }

    /* Intent: probe_pool propagates malformed mountinfo as a MountInfo
     *   error instead of treating the pool as offline.
     * Why: parser uncertainty must fail closed in the safety-critical probe.
     * Scenario: the target line is malformed.
     */
    #[test]
    fn probe_pool_propagates_mountinfo_malformed_line() {
        let fs = MockFs::with_mountinfo(&malformed_mountinfo_for_target());
        let runner = MockRunner::default();

        let result = probe_pool(&runner, &fs, &mp());
        assert!(matches!(
            result,
            Err(ProbeError::MountInfo(
                crate::mount_check::MountInfoError::Malformed { .. }
            ))
        ));
    }

    // Intent: probe_pool_alerts reports an unmounted pool without running
    // any subprocesses.
    // Why it exists: ack and monitor treat genuine mount absence as offline,
    // so the narrowed probe must preserve probe_pool's offline contract.
    // Scenario: the NAS has booted but the encrypted pool has not been
    // unlocked and mounted yet.
    #[test]
    fn probe_pool_alerts_unmounted() {
        let fs = MockFs::with_mountinfo(&mountinfo_without_target());
        let runner = MockRunner::default();

        let result = probe_pool_alerts(&runner, &fs, &mp()).unwrap();

        assert!(!result.mounted);
        assert!(result.present_devids.is_empty());
        assert!(result.missing_devids.is_empty());
        assert!(result.null_underlying_devids.is_empty());
        assert!(runner.requests().is_empty());
    }

    // Intent: probe_pool_alerts observes a healthy mounted two-disk pool
    // using btrfs-show plus one cryptsetup-status call per device.
    // Why it exists: the alert pipeline only needs btrfs devids; issuing
    // cryptsetup luksUUID here would re-expand the failure surface this
    // refactor removes.
    // Scenario: ack or monitor runs while both encrypted pool members are
    // mounted and active.
    #[test]
    fn probe_pool_alerts_mounted_2disk() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_2disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            );

        let result = probe_pool_alerts(&runner, &fs, &mp()).unwrap();

        assert!(result.mounted);
        assert_eq!(result.present_devids, vec![Devid::new(1), Devid::new(2)]);
        assert!(result.missing_devids.is_empty());
        assert!(result.null_underlying_devids.is_empty());
        assert_eq!(
            result.fsid.as_ref().map(Fsid::as_str),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "alert probe must carry the FS UUID for the ENOSPC baseline key"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksUuid { .. })),
            "alert probe must not request cryptsetup luksUUID"
        );
    }

    // Intent: probe_pool_alerts carries btrfs MISSING sentinel devids without
    // trying to probe a missing mapper.
    // Why it exists: MissingDevice alerts are keyed by btrfs devid and must
    // not depend on LUKS identity for a device btrfs already marks missing.
    // Scenario: one drive has disappeared and btrfs filesystem show reports
    // its devid with a MISSING sentinel.
    #[test]
    fn probe_pool_alerts_with_btrfs_missing_sentinel() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 0 used 0 path /dev/mapper/braid-ironwolf MISSING\n\
                     \t*** Some devices missing\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            );

        let result = probe_pool_alerts(&runner, &fs, &mp()).unwrap();

        assert!(result.mounted);
        assert_eq!(result.present_devids, vec![Devid::new(1)]);
        assert_eq!(result.missing_devids, vec![Devid::new(2)]);
        assert!(result.null_underlying_devids.is_empty());
    }

    // Intent: probe_pool_alerts records active mappers whose backing device
    // has become `(null)` separately from btrfs MISSING sentinel devids.
    // Why it exists: alerting should fire for hot-unplugged backing devices
    // while destructive remove-missing logic stays tied to btrfs MISSING.
    // Scenario: one pool mapper remains open after its underlying block
    // device has disappeared.
    #[test]
    fn probe_pool_alerts_with_null_underlying() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_2disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                cryptsetup_status_active_null("braid-ironwolf"),
            );

        let result = probe_pool_alerts(&runner, &fs, &mp()).unwrap();

        assert!(result.mounted);
        assert_eq!(result.present_devids, vec![Devid::new(1)]);
        assert!(result.missing_devids.is_empty());
        assert_eq!(result.null_underlying_devids, vec![Devid::new(2)]);
    }

    // Intent: probe_pool_alerts does not reject mounted btrfs-show output
    // solely because the FSID line is absent.
    // Why it exists: ack and monitor do not consume the pool FSID; retaining
    // probe_pool's FSID rejection would keep an unnecessary failure path in
    // the alert pipeline.
    // Scenario: btrfs filesystem show still reports total devices and mapper
    // rows, but the uuid line is absent from the payload.
    #[test]
    fn probe_pool_alerts_tolerates_missing_fsid() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "\tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            );

        let result = probe_pool_alerts(&runner, &fs, &mp()).unwrap();

        assert!(result.mounted);
        assert_eq!(result.present_devids, vec![Devid::new(1), Devid::new(2)]);
        assert!(result.missing_devids.is_empty());
        assert!(result.null_underlying_devids.is_empty());
        assert_eq!(
            result.fsid, None,
            "an absent uuid line yields no usable PoolKey identity"
        );
    }

    // Intent: probe_pool_alerts preserves the typed NotBtrfs response when
    // the configured mount point is occupied by another filesystem.
    // Why it exists: ack and monitor classify foreign mounts differently
    // from indeterminate probe failures.
    // Scenario: /mnt/storage is currently an ext4 mount, not the btrfs pool.
    #[test]
    fn probe_pool_alerts_not_btrfs() {
        let fs = MockFs::with_mountinfo(&mountinfo_ext4());
        let runner = MockRunner::default();

        let err = probe_pool_alerts(&runner, &fs, &mp()).unwrap_err();

        assert!(
            matches!(err, ProbeError::NotBtrfs { ref fstype, .. } if fstype == "ext4"),
            "expected ProbeError::NotBtrfs, got: {err:?}"
        );
    }

    // Intent: probe_pool_alerts rejects a mapper listed by btrfs when
    // cryptsetup status says that mapper is inactive.
    // Why it exists: the alert probe still depends on cryptsetup status to
    // distinguish present devices from null-underlying or inconsistent state.
    // Scenario: btrfs filesystem show names /dev/mapper/braid-toshiba, but
    // cryptsetup status reports the mapper as inactive.
    #[test]
    fn probe_pool_alerts_mapper_not_active() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                err_raw(
                    "cryptsetup status braid-toshiba",
                    4,
                    "/dev/mapper/braid-toshiba is not active.\n",
                ),
            );

        let err = probe_pool_alerts(&runner, &fs, &mp()).unwrap_err();

        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. } if detail == "not active"),
            "expected ProbeError::PoolDevice not active, got: {err:?}"
        );
    }

    // Intent: probe_pool_alerts rejects real btrfs device paths that are not
    // /dev/mapper paths.
    // Why it exists: braid pools are expected to sit behind LUKS mappers, and
    // the narrowed alert probe must not weaken that invariant.
    // Scenario: btrfs filesystem show reports a raw block path for a mounted
    // pool member.
    #[test]
    fn probe_pool_alerts_non_mapper_device() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            ok_raw(
                "btrfs filesystem show /mnt/storage",
                "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/sda1\n",
            ),
        );

        let err = probe_pool_alerts(&runner, &fs, &mp()).unwrap_err();

        assert!(
            matches!(err, ProbeError::PoolDevice { ref mapper, ref detail }
                if mapper == "/dev/sda1" && detail == "not a /dev/mapper/ path"),
            "expected ProbeError::PoolDevice non-mapper, got: {err:?}"
        );
    }

    // Intent: probe_pool_alerts propagates mountinfo IO errors instead of
    // treating the pool as offline.
    // Why it exists: monitor must fail closed when mount state is
    // indeterminate, and ack must not clear alerts from an unreadable mount
    // probe.
    // Scenario: /proc/self/mountinfo cannot be read.
    #[test]
    fn probe_pool_alerts_propagates_mountinfo_io_error() {
        let fs = MockFs::with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let runner = MockRunner::default();

        let result = probe_pool_alerts(&runner, &fs, &mp());

        assert!(matches!(
            result,
            Err(ProbeError::MountInfo(
                crate::mount_check::MountInfoError::Io(_)
            ))
        ));
    }

    // Intent: AlertPoolState::alert_devids() populates AlertDevids::missing with
    // the alert-local union of btrfs MISSING devids and null-underlying devids.
    // Why it exists: deduplication and sorting are part of the alert cause
    // contract, not a property callers should reimplement.
    // Scenario: btrfs has promoted one devid to MISSING while another mapper
    // still reports a null underlying, with one devid present in both inputs.
    #[test]
    fn probe_pool_alerts_alert_devids_missing() {
        let state = AlertPoolState {
            mounted: true,
            present_devids: vec![Devid::new(1)],
            missing_devids: vec![Devid::new(4), Devid::new(2)],
            null_underlying_devids: vec![Devid::new(2), Devid::new(3)],
            fsid: None,
        };

        assert_eq!(
            state.alert_devids().missing,
            BTreeSet::from([Devid::new(2), Devid::new(3), Devid::new(4)])
        );
    }

    // Intent: AlertPoolState::alert_devids() populates AlertDevids::recognized
    //   with the alert-pipeline union of present, btrfs-MISSING, and
    //   null-underlying devids.
    // Why it exists: monitor and ack must filter stats rows against the same
    //   sorted, deduped membership set instead of rebuilding it separately.
    // Scenario: one devid is present, one is both MISSING and null-backed, and
    //   another exists only as null-backed.
    #[test]
    fn probe_pool_alerts_recognized_devids() {
        let state = AlertPoolState {
            mounted: true,
            present_devids: vec![Devid::new(4), Devid::new(1)],
            missing_devids: vec![Devid::new(4), Devid::new(2)],
            null_underlying_devids: vec![Devid::new(2), Devid::new(3)],
            fsid: None,
        };

        assert_eq!(
            state.alert_devids().recognized,
            BTreeSet::from([Devid::new(1), Devid::new(2), Devid::new(3), Devid::new(4)])
        );
    }

    // Intent: AlertPoolState::alert_devids() groups the three devid origins
    //   (present, btrfs-MISSING, null-underlying) into a carrier whose
    //   `recognized` and `missing` fields have distinct semantics; feeding that
    //   carrier through compute_alert_state routes each devid to its correct
    //   alert outcome. A backwards builder that swaps
    //   `recognized` and `missing` would flip every assertion below.
    // Why it exists: the positional two-slice API was replaced by a single
    //   named carrier to make a recognized/missing swap unwritable at the call
    //   site. This test proves alert_devids() fills the fields in the right
    //   order -- a builder that swapped them would still compile, so a pure
    //   compile check is insufficient.
    // Scenario: a three-disk pool where disk 1 is present, disk 2 has
    //   disappeared (btrfs MISSING sentinel), and disk 3 is a hot-unplugged
    //   null-underlying mapper. All three devid origins are distinct.
    #[test]
    fn alert_devids_carrier_routes_each_origin_correctly() {
        use crate::alert::{AckedStats, AlertCause, compute_alert_state};

        // devid 1: present (normal, healthy member)
        // devid 2: btrfs-MISSING (kernel lost sight of the disk entirely)
        // devid 3: null-underlying (mapper open, backing block device gone)
        let state = AlertPoolState {
            mounted: true,
            present_devids: vec![Devid::new(1)],
            missing_devids: vec![Devid::new(2)],
            null_underlying_devids: vec![Devid::new(3)],
            fsid: None,
        };

        let devids = state.alert_devids();

        // recognized must contain all three origins; missing must exclude present.
        assert_eq!(
            devids.recognized,
            BTreeSet::from([Devid::new(1), Devid::new(2), Devid::new(3)]),
            "recognized must be present union null-underlying union btrfs-MISSING"
        );
        assert_eq!(
            devids.missing,
            BTreeSet::from([Devid::new(2), Devid::new(3)]),
            "missing must be btrfs-MISSING union null-underlying"
        );

        // Btrfs device stats rows for all three devids (all zero counters).
        use crate::parse::types::{BtrfsDeviceStatsOutput, DeviceErrorStats};
        let stats = BtrfsDeviceStatsOutput {
            devices: vec![
                DeviceErrorStats {
                    devid: Devid::new(1),
                    read_io_errs: 0,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 0,
                    generation_errs: 0,
                },
                DeviceErrorStats {
                    devid: Devid::new(2),
                    read_io_errs: 0,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 0,
                    generation_errs: 0,
                },
                DeviceErrorStats {
                    devid: Devid::new(3),
                    read_io_errs: 0,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 0,
                    generation_errs: 0,
                },
            ],
        };
        let acked = AckedStats::default();

        // compute_alert_state must produce MissingDevice for devid 2 and 3
        // (the missing set) and no BtrfsDeviceErrors for devid 1.
        let alert = compute_alert_state(&stats, &acked, &devids, false, false);
        assert!(
            alert.contains(&AlertCause::MissingDevice {
                devid: Devid::new(2),
                still_missing: true
            }),
            "btrfs-MISSING devid must latch MissingDevice"
        );
        assert!(
            alert.contains(&AlertCause::MissingDevice {
                devid: Devid::new(3),
                still_missing: true
            }),
            "null-underlying devid must latch MissingDevice"
        );
        assert!(
            !alert.iter().any(|c| matches!(
                c,
                AlertCause::BtrfsDeviceErrors { devid, .. } if *devid == Devid::new(1)
            )),
            "present devid must not produce BtrfsDeviceErrors"
        );
        assert!(
            !alert.iter().any(|c| matches!(
                c,
                AlertCause::MissingDevice { devid, .. } if *devid == Devid::new(1)
            )),
            "present devid must not produce MissingDevice"
        );
    }

    // -- probe_fsid tests --

    // Intent: probe_fsid returns the FSID using mountinfo plus exactly one
    //   subprocess (btrfs filesystem show).
    // Why: cmd_lock's preflight only needs the FSID. If probe_fsid
    //   silently starts issuing per-device cryptsetup calls it would
    //   regress the point of the refactor (fewer subprocesses, smaller
    //   failure surface). MockRunner panics on any CmdRequest without a
    //   registered mock, so registering only BtrfsFilesystemShow
    //   mechanically proves the helper stays narrow.
    // Scenario: a mounted btrfs pool; probe_fsid extracts the uuid from
    //   btrfs-show output without touching cryptsetup.
    #[test]
    fn probe_fsid_happy() {
        let fs = MockFs::with_mountinfo(&mountinfo_btrfs());
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            btrfs_show_2disk(),
        );

        let fsid = probe_fsid(&runner, &fs, &mp()).unwrap();
        assert_eq!(fsid.as_str(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    // Intent: probe_fsid preserves probe_pool's NotBtrfs contract.
    // Why: cmd_lock relies on this typed error to report "this mount is
    //   not ours" rather than a generic btrfs-show parse failure.
    //   Downgrading NotBtrfs to a plain command failure would degrade
    //   the user-facing message on a mis-configured mount.
    // Scenario: mountinfo reports the mount point is ext4. probe_fsid must
    //   reject with ProbeError::NotBtrfs{fstype:"ext4"} before running
    //   btrfs filesystem show.
    #[test]
    fn probe_fsid_rejects_non_btrfs() {
        let fs = MockFs::with_mountinfo(&mountinfo_ext4());
        let runner = MockRunner::default();

        let err = probe_fsid(&runner, &fs, &mp()).unwrap_err();
        assert!(
            matches!(err, ProbeError::NotBtrfs { ref fstype, .. } if fstype == "ext4"),
            "expected ProbeError::NotBtrfs, got: {err:?}"
        );
    }

    // Intent: probe_fsid errors when mountinfo has no entry for the
    //   mount point (e.g. the mount raced to unmount between
    //   MountpointCheck and probe_fsid).
    // Why: silently returning "no FSID" or some default would let
    //   cmd_lock dereference a non-existent sysfs path and either
    //   succeed vacuously or yield a confusing I/O error. A typed
    //   PoolDevice error surfaces the state clearly.
    // Scenario: mountinfo is readable but has no /mnt/storage entry.
    #[test]
    fn probe_fsid_target_absent_in_mountinfo() {
        let fs = MockFs::with_mountinfo(&mountinfo_without_target());
        let runner = MockRunner::default();

        let err = probe_fsid(&runner, &fs, &mp()).unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. }
                if detail.contains("not present in mountinfo")),
            "expected ProbeError::PoolDevice (mount not in mountinfo), got: {err:?}"
        );
    }

    // Intent: probe_fsid propagates mountinfo IO errors.
    // Why: lock preflight must not proceed from indeterminate mount state.
    // Scenario: /proc/self/mountinfo cannot be read.
    #[test]
    fn probe_fsid_propagates_mountinfo_io_error() {
        let fs = MockFs::with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let runner = MockRunner::default();

        let err = probe_fsid(&runner, &fs, &mp()).unwrap_err();
        assert!(matches!(
            err,
            ProbeError::MountInfo(crate::mount_check::MountInfoError::Io(_))
        ));
    }

    // Intent: probe_fsid propagates malformed mountinfo.
    // Why: malformed mountinfo must not be coerced into "not mounted".
    // Scenario: target line is missing the dash separator.
    #[test]
    fn probe_fsid_propagates_malformed_mountinfo() {
        let fs = MockFs::with_mountinfo(&malformed_mountinfo_for_target());
        let runner = MockRunner::default();

        let err = probe_fsid(&runner, &fs, &mp()).unwrap_err();
        assert!(matches!(
            err,
            ProbeError::MountInfo(crate::mount_check::MountInfoError::Malformed { .. })
        ));
    }

    // Intent: probe_fsid propagates duplicate target entries.
    // Why: overmount ambiguity must fail closed before lock preflight.
    // Scenario: two mountinfo entries report the configured mount point.
    #[test]
    fn probe_fsid_propagates_duplicate_target() {
        let fs = MockFs::with_mountinfo(&duplicate_mountinfo_for_target());
        let runner = MockRunner::default();

        let err = probe_fsid(&runner, &fs, &mp()).unwrap_err();
        assert!(matches!(
            err,
            ProbeError::MountInfo(crate::mount_check::MountInfoError::DuplicateTarget { .. })
        ));
    }
}
