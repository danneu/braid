use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, Config, ConfigError};
use crate::luks::{self, LuksError};
use crate::pool::PoolError;
use crate::probe::{self, Filesystem, ProbeError};
use crate::types::ConfigDiskState;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Luks(#[from] LuksError),
    #[error("{0}")]
    Pool(#[from] PoolError),
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    key_file: Option<&std::path::Path>,
) -> Result<(), UnlockError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted → print message, exit 0
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.to_owned(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(());
    }

    // 2. Probe each config disk
    let mut to_unlock = Vec::new(); // (name, disk) pairs needing unlock
    let mut any_open = false;
    let mut any_absent = false;
    let mut any_not_luks = false;

    for (name, disk) in config.disks() {
        let probed = probe::probe_config_disk(runner, fs, name, disk)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
                any_absent = true;
            }
            ConfigDiskState::PresentNotLuks => {
                eprintln!(
                    "{}  disk: {:<10}not initialized, run `braid add {}`",
                    tag("skip"),
                    name,
                    name
                );
                any_not_luks = true;
            }
            ConfigDiskState::PresentLuks {
                mapper_open: true, ..
            } => {
                eprintln!("{}  disk: {:<10}already open", tag("ok"), name);
                any_open = true;
            }
            ConfigDiskState::PresentLuks {
                mapper_open: false, ..
            } => {
                eprintln!("{}  disk: {:<10}found", tag("ok"), name);
                to_unlock.push((name.clone(), disk.clone()));
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        if any_not_luks {
            return Err(UnlockError::Failed(
                "no unlockable disks found; some disks are not initialized (run `braid add`)"
                    .into(),
            ));
        }
        return Err(UnlockError::Failed("no unlockable disks found".into()));
    }

    // 4. If disks need opening → verify credential, then open each disk
    if !to_unlock.is_empty() {
        if let Some(kf) = key_file {
            // Keyfile path: verify against first disk, then open each
            let (ref first_name, ref first_disk) = to_unlock[0];
            let ok = luks::verify_key_file(runner, &first_disk.by_id.0, kf)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong keyfile (verified against {})",
                    first_name
                )));
            }

            for (name, disk) in &to_unlock {
                luks::ensure_luks_open_with_key_file(runner, fs, name, disk, kf)?;
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
            }
        } else {
            // Passphrase path (unchanged)
            let passphrase = luks::read_passphrase(passphrase_file, passphrase_stdin)?;

            let (ref first_name, ref first_disk) = to_unlock[0];
            let ok = luks::verify_passphrase(runner, &first_disk.by_id.0, &passphrase)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong passphrase (verified against {})",
                    first_name
                )));
            }

            for (name, disk) in &to_unlock {
                luks::ensure_luks_open(runner, fs, name, disk, &passphrase)?;
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
            }
        }
    }

    // 5. btrfs device scan
    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    if scan.exit_status != 0 {
        return Err(UnlockError::Failed(format!(
            "btrfs device scan failed (exit {}): {}",
            scan.exit_status,
            scan.stderr.trim()
        )));
    }

    // 6. mkdir -p mount_point, then mount
    let _ = std::fs::create_dir_all(mount_point);

    let mount_result = if any_absent || any_not_luks {
        // Some disks missing → degraded mount
        runner.run(&CmdRequest::MountWithOptions {
            device: format!(
                "/dev/mapper/{}",
                mapper_name(
                    &to_unlock
                        .first()
                        .map(|(k, _)| k.as_str())
                        .or_else(|| {
                            // All disks were already open — find first open mapper
                            config.disks().keys().next().map(|k| k.as_str())
                        })
                        .unwrap_or("unknown")
                )
                .0
            ),
            mount_point: mount_point.to_owned(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        // All disks present — find a mapper device to mount from
        let mount_key = to_unlock
            .first()
            .map(|(k, _)| k.as_str())
            .or_else(|| config.disks().keys().next().map(|k| k.as_str()))
            .unwrap_or("unknown");
        runner.run(&CmdRequest::Mount {
            device: format!("/dev/mapper/{}", mapper_name(mount_key).0),
            mount_point: mount_point.to_owned(),
        })?
    };

    if mount_result.exit_status != 0 {
        return Err(UnlockError::Failed(format!(
            "mount failed (exit {}): {}",
            mount_result.exit_status,
            mount_result.stderr.trim()
        )));
    }

    eprintln!("{}  {:<10}mounted {}", tag("ok"), "pool", mount_point);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::config::{Config, DiskConfig};
    use crate::types::ByIdPath;
    use std::collections::BTreeMap;

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
    }

    fn ok_raw(cmd: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
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

    fn three_disk_config() -> Config {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
            ("disk3", "/dev/disk/by-id/virtio-disk3"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskConfig {
                    by_id: ByIdPath(path.to_owned()),
                },
            );
        }
        Config::new(disks, "/mnt/storage".to_owned()).unwrap()
    }

    /// Bricked LUKS header (PresentNotLuks) must trigger degraded mount.
    ///
    /// Scenario: 3-disk RAID1, disk3's LUKS header is zeroed. Probe sees disk3
    /// as PresentNotLuks (device exists, but cryptsetup luksUUID fails). The
    /// surviving 2 disks unlock normally. Mount must use `-o degraded` because
    /// btrfs will see a missing member device.
    #[test]
    fn unlock_bricked_disk_uses_degraded_mount() {
        let config = three_disk_config();

        // disk1 & disk2: exist, are LUKS, mapper not yet open
        // disk3: exists but not LUKS (bricked header)
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default()
            // 1. mountpoint check → not mounted
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".into(),
                },
                err_raw("mountpoint", 1, ""),
            )
            // 2. probe: disk1 is LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: "aaaaaaaa-1111-2222-3333-444444444444\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            // 2. probe: disk2 is LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: "bbbbbbbb-1111-2222-3333-444444444444\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            // 2. probe: disk3 NOT LUKS (bricked)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                err_raw(
                    "cryptsetup luksUUID",
                    1,
                    "Device is not a valid LUKS device.",
                ),
            )
            // 4. verify passphrase against first unlockable disk
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // 4. open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // 4. open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // 5. btrfs device scan
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            // 6. mount WITH degraded (this is what the test asserts)
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: "/mnt/storage".into(),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o degraded"),
            );

        // Write passphrase to a temp file for the test (avoid stdin TTY)
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(&runner, &fs, &config, false, Some(tmp.path()), None);

        // If the code incorrectly uses Mount instead of MountWithOptions,
        // MockRunner returns MissingMock → the test fails.
        result.expect("unlock with bricked disk should use degraded mount and succeed");
    }
}
