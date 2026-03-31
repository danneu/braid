use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::mapper_name;
use crate::parse::{
    parse_btrfs_filesystem_show, parse_cryptsetup_luks_uuid, parse_cryptsetup_status, ParseError,
};
use crate::types::*;

// ---------------------------------------------------------------------------
// Filesystem trait — abstracts Path::exists() for testability
// ---------------------------------------------------------------------------

pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
    fn is_block_device(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
}

pub struct RealFilesystem;

impl Filesystem for RealFilesystem {
    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn is_block_device(&self, path: &str) -> bool {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path)
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    names.push(entry?.file_name().to_string_lossy().into_owned());
                }
                Ok(names)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }
}

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
}

// ---------------------------------------------------------------------------
// probe_config_disk
// ---------------------------------------------------------------------------

pub fn probe_config_disk<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &str,
    by_id: &ByIdPath,
) -> Result<ConfigDisk, ProbeError> {
    if !fs.exists(&by_id.0) {
        return Ok(ConfigDisk {
            name: name.to_owned(),
            by_id_path: by_id.clone(),
            state: ConfigDiskState::Absent,
        });
    }

    let raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: by_id.0.clone(),
    })?;

    let uuid = match parse_cryptsetup_luks_uuid(&raw) {
        Ok(out) => out.uuid,
        Err(ParseError::CommandFailed { .. }) => {
            return Ok(ConfigDisk {
                name: name.to_owned(),
                by_id_path: by_id.clone(),
                state: ConfigDiskState::PresentNotLuks,
            });
        }
        Err(e) => return Err(ProbeError::Parse(e)),
    };

    let mn = mapper_name(name);
    let mapper_open = fs.exists(&format!("/dev/mapper/{}", mn.0));

    Ok(ConfigDisk {
        name: name.to_owned(),
        by_id_path: by_id.clone(),
        state: ConfigDiskState::PresentLuks { uuid, mapper_open },
    })
}

// ---------------------------------------------------------------------------
// probe_pool
// ---------------------------------------------------------------------------

pub fn probe_pool<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<PoolState, ProbeError> {
    let findmnt_raw = runner.run(&CmdRequest::FindmntJson {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let findmnt = crate::parse::parse_findmnt_json(&findmnt_raw)?;

    // Defensive: only consider entries whose target exactly matches mount_point.
    let exact = findmnt.filesystems.iter().find(|e| e.target == mount_point);

    let entry = match exact {
        None => {
            return Ok(PoolState {
                mounted: false,
                devices: vec![],
                missing_count: 0,
                total_devices: 0,
                fsid: None,
                missing_devids: vec![],
            });
        }
        Some(e) => e,
    };

    if entry.fstype != "btrfs" {
        return Err(ProbeError::NotBtrfs {
            mount_point: mount_point.to_owned(),
            fstype: entry.fstype.clone(),
        });
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let show = parse_btrfs_filesystem_show(&show_raw)?;

    // A mounted btrfs filesystem always has an FSID. None here means the
    // parser couldn't extract the uuid line — a broken invariant, not a
    // state we should silently propagate to consumers.
    let fsid = show.uuid.ok_or_else(|| ProbeError::PoolDevice {
        mapper: mount_point.to_owned(),
        detail: "mounted pool has no FSID in btrfs filesystem show output".into(),
    })?;

    let mut devices = Vec::new();
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
            mapper: name.clone(),
        })?;
        let status = parse_cryptsetup_status(&status_raw)?;

        if !status.is_active {
            return Err(ProbeError::PoolDevice {
                mapper: name,
                detail: "not active".to_owned(),
            });
        }

        // When a backing device is hot-unplugged, cryptsetup reports
        // device: (null). Skip these — the device is effectively gone.
        let underlying = match status.device {
            None => continue,
            Some(ref d) if d == "(null)" => continue,
            Some(d) => d,
        };

        let uuid_raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
            device: underlying.clone(),
        })?;
        let uuid_out = parse_cryptsetup_luks_uuid(&uuid_raw)?;

        devices.push(PoolDevice {
            mapper: MapperName(name),
            luks_uuid: uuid_out.uuid,
            devid: bdev.devid,
            underlying,
        });
    }

    let missing_count = show.total_devices.saturating_sub(devices.len() as u64);

    Ok(PoolState {
        mounted: true,
        devices,
        missing_count,
        total_devices: show.total_devices,
        fsid: Some(fsid),
        missing_devids: show.missing_devids,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    struct MockFs {
        paths: Vec<String>,
        block_devices: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, path: &str) -> bool {
            self.block_devices.contains(&path.to_string())
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
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
        ByIdPath(path.to_owned())
    }

    // -- probe_config_disk tests --

    #[test]
    fn probe_config_disk_absent() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(&runner, &fs, "toshiba", &d).unwrap();
        assert_eq!(result.name, "toshiba");
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

        let result = probe_config_disk(&runner, &fs, "toshiba", &d).unwrap();
        assert_eq!(result.state, ConfigDiskState::PresentNotLuks);
    }

    #[test]
    fn probe_config_disk_cmd_spawn_fails() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(&runner, &fs, "toshiba", &d);
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

        let result = probe_config_disk(&runner, &fs, "toshiba", &d);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::Parse(_)),
            "expected ProbeError::Parse, got: {err:?}"
        );
    }

    #[test]
    fn probe_config_disk_present_luks_closed() {
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

        let result = probe_config_disk(&runner, &fs, "toshiba", &d).unwrap();
        assert_eq!(result.name, "toshiba");
        assert_eq!(
            result.state,
            ConfigDiskState::PresentLuks {
                uuid: LuksUuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
                mapper_open: false,
            }
        );
    }

    #[test]
    fn probe_config_disk_present_luks_open() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk-1".into(),
            },
            ok_raw(
                "cryptsetup luksUUID /dev/disk/by-id/disk-1",
                "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n",
            ),
        );
        // Named mapper: braid-toshiba
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1", "/dev/mapper/braid-toshiba"]);
        let d = by_id("/dev/disk/by-id/disk-1");

        let result = probe_config_disk(&runner, &fs, "toshiba", &d).unwrap();
        assert_eq!(
            result.state,
            ConfigDiskState::PresentLuks {
                uuid: LuksUuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
                mapper_open: true,
            }
        );
    }

    // -- probe_pool tests --

    fn findmnt_empty() -> RawCommandOutput {
        err_raw(
            "findmnt --json --output TARGET,SOURCE,FSTYPE -T /mnt/storage",
            1,
            "",
        )
    }

    fn findmnt_btrfs() -> RawCommandOutput {
        ok_raw(
            "findmnt --json --output TARGET,SOURCE,FSTYPE -T /mnt/storage",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
        )
    }

    fn findmnt_ext4() -> RawCommandOutput {
        ok_raw(
            "findmnt --json --output TARGET,SOURCE,FSTYPE -T /mnt/storage",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/sda1","fstype":"ext4"}]}"#,
        )
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

    fn cryptsetup_uuid_ok(device: &str, uuid: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup luksUUID {device}"),
            &format!("{uuid}\n"),
        )
    }

    #[test]
    fn probe_pool_unmounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            findmnt_empty(),
        );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(!result.mounted);
        assert!(result.devices.is_empty());
        assert_eq!(result.missing_count, 0);
    }

    #[test]
    fn probe_pool_unmounted_target_mismatch() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw(
                "findmnt --json --output TARGET,SOURCE,FSTYPE --mountpoint /mnt/storage",
                r#"{"filesystems": [{"target":"/","source":"/dev/sda1","fstype":"ext4"}]}"#,
            ),
        );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(!result.mounted);
        assert!(result.devices.is_empty());
        assert_eq!(result.missing_count, 0);
    }

    #[test]
    fn probe_pool_mounted_not_btrfs() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            findmnt_ext4(),
        );

        let result = probe_pool(&runner, "/mnt/storage");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::NotBtrfs { ref fstype, .. } if fstype == "ext4"),
            "expected ProbeError::NotBtrfs, got: {err:?}"
        );
    }

    #[test]
    fn probe_pool_mounted_2disk() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_2disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-toshiba".into(),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-ironwolf".into(),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 2);
        assert_eq!(result.missing_count, 0);
        assert_eq!(result.total_devices, 2);
        assert_eq!(result.devices[0].mapper, MapperName("braid-toshiba".into()));
        assert_eq!(
            result.devices[0].luks_uuid,
            LuksUuid("11111111-1111-1111-1111-111111111111".into())
        );
        assert_eq!(result.devices[0].devid, 1);
        assert_eq!(
            result.devices[1].mapper,
            MapperName("braid-ironwolf".into())
        );
        assert_eq!(
            result.fsid.as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "pool FSID must be populated from btrfs filesystem show"
        );
    }

    #[test]
    fn probe_pool_mounted_with_missing() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_3disk_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-toshiba".into(),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-ironwolf".into(),
                },
                cryptsetup_status_active("braid-ironwolf", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 2);
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 3);
    }

    #[test]
    fn probe_pool_degraded_missing_sentinel() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
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
                    mapper: "braid-toshiba".into(),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(result.mounted);
        assert_eq!(result.devices.len(), 1, "MISSING device must be excluded");
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 2);
        assert_eq!(result.devices[0].mapper, MapperName("braid-toshiba".into()));
    }

    #[test]
    fn probe_pool_mapper_not_active() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
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
                    mapper: "braid-toshiba".into(),
                },
                err_raw(
                    "cryptsetup status braid-toshiba",
                    4,
                    "/dev/mapper/braid-toshiba is not active.\n",
                ),
            );

        let result = probe_pool(&runner, "/mnt/storage");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. } if detail == "not active"),
            "expected ProbeError::PoolDevice not active, got: {err:?}"
        );
    }

    #[test]
    fn probe_pool_non_mapper_device() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/sda1\n",
                ),
            );

        let result = probe_pool(&runner, "/mnt/storage");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref mapper, ref detail }
                if mapper == "/dev/sda1" && detail == "not a /dev/mapper/ path"),
            "expected ProbeError::PoolDevice non-mapper, got: {err:?}"
        );
    }

    /// Hot-unplugged device: btrfs still lists the mapper path, but
    /// cryptsetup status reports `device: (null)` because the underlying
    /// block device is gone. probe_pool must skip this device instead of
    /// crashing on `cryptsetup luksUUID (null)`.
    #[test]
    fn probe_pool_device_null_underlying() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
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
                    mapper: "braid-toshiba".into(),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-ironwolf".into(),
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

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
        assert!(result.mounted);
        assert_eq!(
            result.devices.len(),
            1,
            "device with (null) underlying must be skipped"
        );
        assert_eq!(result.devices[0].mapper, MapperName("braid-toshiba".into()));
        assert_eq!(result.missing_count, 1);
        assert_eq!(result.total_devices, 2);
    }

    #[test]
    fn probe_pool_missing_count_saturates() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
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
                    mapper: "braid-toshiba".into(),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            );

        let result = probe_pool(&runner, "/mnt/storage").unwrap();
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
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "\tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            );

        let result = probe_pool(&runner, "/mnt/storage");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProbeError::PoolDevice { ref detail, .. }
                if detail.contains("no FSID")),
            "expected ProbeError::PoolDevice about missing FSID, got: {err:?}"
        );
    }
}
