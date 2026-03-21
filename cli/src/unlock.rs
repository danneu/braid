use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{Config, ConfigError, mapper_name};
use crate::disk_map::{self, DiskMap};
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
    #[error("{0}")]
    DegradedRefused(String),
    #[error("{0}")]
    NameStability(#[from] crate::disk_map::NameStabilityError),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    disk_map: &DiskMap,
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    key_file: Option<&std::path::Path>,
    allow_degraded: bool,
) -> Result<(), UnlockError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted → print message, exit 0
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(());
    }

    // 2. Validate config/disk-map identity consistency
    crate::disk_map::validate_config_name_stability(config, disk_map)?;

    // 3. Probe each config disk
    let mut to_unlock = Vec::new(); // (name, disk) pairs needing unlock
    let mut any_open = false;
    let mut any_missing_member = false;
    let mut any_uninitialized = false;

    for (name, disk) in config.disks() {
        let probed = probe::probe_config_disk(runner, fs, name, disk)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                if disk_map.disks.contains_key(name) {
                    // Known pool member, confirmed missing → degradable
                    eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
                    any_missing_member = true;
                } else {
                    // Never added — config error, not a degraded scenario
                    eprintln!(
                        "{}  disk: {:<10}not found and never added to pool, run `braid add {}`",
                        tag("skip"),
                        name,
                        name
                    );
                    any_uninitialized = true;
                }
            }
            ConfigDiskState::PresentNotLuks => {
                if disk_map.disks.contains_key(name) {
                    // Was a pool member, LUKS header now bricked → degradable
                    eprintln!(
                        "{}  disk: {:<10}LUKS header damaged (was pool member)",
                        tag("skip"),
                        name
                    );
                    any_missing_member = true;
                } else {
                    // Never was a pool member — genuinely uninitialized
                    eprintln!(
                        "{}  disk: {:<10}not initialized, run `braid add {}`",
                        tag("skip"),
                        name,
                        name
                    );
                    any_uninitialized = true;
                }
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
        if any_uninitialized {
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
                luks::ensure_luks_open_with_key_file(runner, fs, name, disk, kf).map_err(|_| {
                    UnlockError::Failed(format!(
                        "failed to open disk '{}': keyfile was verified against \
                             '{}' but rejected here (single-passphrase invariant \
                             may be violated by external LUKS manipulation)",
                        name, first_name
                    ))
                })?;
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
                luks::ensure_luks_open(runner, fs, name, disk, &passphrase).map_err(|_| {
                    UnlockError::Failed(format!(
                        "failed to open disk '{}': passphrase was verified \
                             against '{}' but rejected here (single-passphrase \
                             invariant may be violated by external LUKS \
                             manipulation)",
                        name, first_name
                    ))
                })?;
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

    // Uninitialized disks are always a hard error — not a degraded scenario
    if any_uninitialized {
        return Err(UnlockError::Failed(
            "some disks are not initialized (run `braid add`)".into(),
        ));
    }

    if any_missing_member && !allow_degraded {
        return Err(UnlockError::DegradedRefused(
            "pool has missing devices — refusing to mount degraded\n\
             new writes would have ZERO redundancy (single-profile chunks)\n\
             hint: braid unlock --allow-degraded"
                .into(),
        ));
    }

    let _ = std::fs::create_dir_all(mount_point);

    let mount_key = to_unlock
        .first()
        .map(|(k, _)| k.as_str())
        .or_else(|| config.disks().keys().next().map(|k| k.as_str()))
        .unwrap_or("unknown");

    let mount_result = if any_missing_member {
        // --allow-degraded was passed and we have confirmed missing pool members
        runner.run(&CmdRequest::MountWithOptions {
            device: format!("/dev/mapper/{}", mapper_name(mount_key).0),
            mount_point: mount_point.clone(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        // All disks present — normal mount
        runner.run(&CmdRequest::Mount {
            device: format!("/dev/mapper/{}", mapper_name(mount_key).0),
            mount_point: mount_point.clone(),
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

    // 7. Best-effort: populate disk-map for any config disks not yet recorded.
    //    Covers migration to a new machine where disk-map.json doesn't exist.
    //    Each entry is verified against the on-disk LUKS label before recording.
    let needs_bootstrap = config
        .disks()
        .keys()
        .any(|name| !disk_map.disks.contains_key(name));
    if needs_bootstrap {
        if let Ok(pool) = probe::probe_pool(runner, mount_point.as_str()) {
            let mut map = disk_map::load_disk_map();
            let mut count = 0u32;

            for (name, disk) in config.disks() {
                if map.disks.contains_key(name) {
                    continue;
                }
                let mn = mapper_name(name);
                let Some(dev) = pool.devices.iter().find(|d| d.mapper == mn) else {
                    continue;
                };

                // Verify on-disk LUKS label matches expected identity.
                // The label (braid-<name>) was written by `braid add` and is the
                // only identity source independent of config.
                let expected_label = format!("braid-{name}");
                let label_ok = runner
                    .run(&CmdRequest::CryptsetupLuksDumpText {
                        device: dev.underlying.clone(),
                    })
                    .ok()
                    .and_then(|raw| crate::parse::parse_cryptsetup_luks_label(&raw).ok())
                    .and_then(|out| out.label)
                    .is_some_and(|label| label == expected_label);

                if label_ok {
                    disk_map::record_disk(
                        &mut map,
                        name,
                        &disk.by_id.0,
                        &dev.luks_uuid.0,
                        dev.devid,
                    );
                    count += 1;
                }
            }

            if count > 0 {
                if let Err(e) = disk_map::save_disk_map(&map) {
                    eprintln!("Warning: failed to save bootstrapped disk map: {e}");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::config::{Config, DiskConfig};
    use crate::disk_map::{DiskMap, DiskMapEntry};
    use crate::types::{ByIdPath, MountPoint};
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
        Config::new(disks, MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    /// Bricked LUKS header (PresentNotLuks) on a known pool member must trigger
    /// degraded mount when --allow-degraded is passed.
    ///
    /// Scenario: 3-disk RAID1, disk3's LUKS header is zeroed. Probe sees disk3
    /// as PresentNotLuks (device exists, but cryptsetup luksUUID fails). The
    /// surviving 2 disks unlock normally. Mount must use `-o degraded` because
    /// btrfs will see a missing member device.
    #[test]
    fn unlock_bricked_disk_uses_degraded_mount() {
        let config = three_disk_config();

        // disk3 is a known pool member in the disk map
        let mut disk_map = DiskMap::new();
        disk_map.disks.insert(
            "disk3".to_owned(),
            DiskMapEntry {
                by_id: "/dev/disk/by-id/virtio-disk3".to_owned(),
                luks_uuid: "cccccccc-1111-2222-3333-444444444444".to_owned(),
                devid: 3,
                added_at: "2024-01-01T00:00:00Z".to_owned(),
            },
        );

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
                    path: MountPoint("/mnt/storage".to_owned()),
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
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o noatime,degraded"),
            );

        // Write passphrase to a temp file for the test (avoid stdin TTY)
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            true, // allow_degraded
        );

        // If the code incorrectly uses Mount instead of MountWithOptions,
        // MockRunner returns MissingMock → the test fails.
        result.expect("unlock with bricked disk should use degraded mount and succeed");
    }

    /// Bricked LUKS header on a known pool member must refuse degraded mount
    /// when --allow-degraded is NOT passed.
    ///
    /// Scenario: Same as unlock_bricked_disk_uses_degraded_mount but without
    /// the flag. The error must tell the user how to proceed.
    #[test]
    fn unlock_bricked_disk_refuses_without_flag() {
        let config = three_disk_config();

        // disk3 is a known pool member
        let mut disk_map = DiskMap::new();
        disk_map.disks.insert(
            "disk3".to_owned(),
            DiskMapEntry {
                by_id: "/dev/disk/by-id/virtio-disk3".to_owned(),
                luks_uuid: "cccccccc-1111-2222-3333-444444444444".to_owned(),
                devid: 3,
                added_at: "2024-01-01T00:00:00Z".to_owned(),
            },
        );

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
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
            // disk3 NOT LUKS (bricked)
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
            // verify passphrase
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // btrfs device scan
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"));
        // No mount mock — should never reach mount

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            false, // allow_degraded = false
        );

        let err = result.expect_err("should refuse degraded mount without --allow-degraded");
        assert!(
            matches!(&err, UnlockError::DegradedRefused(_)),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to mount degraded"),
            "error should mention refusal, got: {msg}"
        );
        assert!(
            msg.contains("--allow-degraded"),
            "error should hint at the flag, got: {msg}"
        );
    }

    /// Uninitialized disk (PresentNotLuks, NOT in disk-map) must be a hard error
    /// even when --allow-degraded is passed.
    ///
    /// Scenario: A disk exists but was never `braid add`'d. It's genuinely
    /// uninitialized, not a bricked pool member. --allow-degraded must not
    /// bypass this check.
    #[test]
    fn unlock_uninitialized_disk_hard_error_even_with_allow_degraded() {
        let config = three_disk_config();

        // disk3 is NOT in the disk map — never added to pool
        let disk_map = DiskMap::new();

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
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
            // disk3: present but NOT LUKS, and NOT in disk map
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
            // verify passphrase
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // btrfs device scan
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"));
        // No mount mock — should never reach mount

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            true, // allow_degraded = true — should NOT bypass uninitialized check
        );

        let err =
            result.expect_err("should fail for uninitialized disk even with --allow-degraded");
        let msg = err.to_string();
        assert!(
            msg.contains("not initialized"),
            "error should mention 'not initialized', got: {msg}"
        );
    }

    /// Identity mismatch between config and disk-map must fail even with
    /// --allow-degraded, before any disk is probed.
    ///
    /// Intent: Name stability enforcement is unconditional — --allow-degraded
    /// only bypasses degraded-mount refusal, never identity mismatches.
    ///
    /// Why it exists: If someone changes a disk's by_id in NixOS config while
    /// the disk-map still has the old value, unlock must refuse. Degraded
    /// classification must not mask this safety violation.
    ///
    /// Scenario: disk3 is absent (unplugged), disk-map has disk3 with a stale
    /// by_id. Even with --allow-degraded, unlock must fail on identity mismatch.
    #[test]
    fn unlock_identity_mismatch_fails_even_with_allow_degraded() {
        let config = three_disk_config();

        // disk-map has disk3 with OLD by_id (mismatch vs config's virtio-disk3)
        let mut disk_map = DiskMap::new();
        disk_map.disks.insert(
            "disk3".to_owned(),
            DiskMapEntry {
                by_id: "/dev/disk/by-id/virtio-disk3-OLD".to_owned(),
                luks_uuid: "cccccccc-1111-2222-3333-444444444444".to_owned(),
                devid: 3,
                added_at: "2024-01-01T00:00:00Z".to_owned(),
            },
        );

        // disk3 is absent — would normally be degradable
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let runner = MockRunner::default()
            // mountpoint check → not mounted
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            );
        // No further mocks — should fail before probing

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            true, // allow_degraded
        );

        let err = result.expect_err("should fail on identity mismatch even with --allow-degraded");
        assert!(
            matches!(
                &err,
                UnlockError::NameStability(
                    crate::disk_map::NameStabilityError::Reassignment { .. }
                )
            ),
            "expected NameStability(Reassignment), got: {err:?}"
        );
    }

    /// Identity mismatch between config and disk-map must fail without
    /// --allow-degraded too.
    ///
    /// Intent: Same enforcement as above, confirming it's not gated on the flag.
    ///
    /// Why it exists: Symmetry test — if the check only ran in the degraded path,
    /// this test would catch it.
    ///
    /// Scenario: Same setup as above but allow_degraded = false.
    #[test]
    fn unlock_identity_mismatch_fails_without_allow_degraded() {
        let config = three_disk_config();

        let mut disk_map = DiskMap::new();
        disk_map.disks.insert(
            "disk3".to_owned(),
            DiskMapEntry {
                by_id: "/dev/disk/by-id/virtio-disk3-OLD".to_owned(),
                luks_uuid: "cccccccc-1111-2222-3333-444444444444".to_owned(),
                devid: 3,
                added_at: "2024-01-01T00:00:00Z".to_owned(),
            },
        );

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            false, // allow_degraded = false
        );

        let err = result.expect_err("should fail on identity mismatch without --allow-degraded");
        assert!(
            matches!(
                &err,
                UnlockError::NameStability(
                    crate::disk_map::NameStabilityError::Reassignment { .. }
                )
            ),
            "expected NameStability(Reassignment), got: {err:?}"
        );
    }

    /// Identity mismatch must fail even when all disks are healthy and present.
    ///
    /// Intent: Proves identity enforcement is unconditional — not tied to
    /// degraded classification or missing disks.
    ///
    /// Why it exists: Without an up-front check, identity mismatches would only
    /// be caught in the Absent/PresentNotLuks branches, letting healthy unlocks
    /// with drifted identities slip through.
    ///
    /// Scenario: All 3 disks present, LUKS-formatted, mapper closed (normal
    /// healthy state). Disk-map has disk1 with a stale by_id.
    #[test]
    fn unlock_identity_mismatch_fails_even_when_all_disks_healthy() {
        let config = three_disk_config();

        // disk-map has disk1 with OLD by_id (mismatch vs config's virtio-disk1)
        let mut disk_map = DiskMap::new();
        disk_map.disks.insert(
            "disk1".to_owned(),
            DiskMapEntry {
                by_id: "/dev/disk/by-id/virtio-disk1-OLD".to_owned(),
                luks_uuid: "aaaaaaaa-1111-2222-3333-444444444444".to_owned(),
                devid: 1,
                added_at: "2024-01-01T00:00:00Z".to_owned(),
            },
        );

        // All disks present
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            false,
        );

        let err = result.expect_err("should fail on identity mismatch even when all disks healthy");
        assert!(
            matches!(
                &err,
                UnlockError::NameStability(
                    crate::disk_map::NameStabilityError::Reassignment { .. }
                )
            ),
            "expected NameStability(Reassignment), got: {err:?}"
        );
    }

    /// Passphrase mismatch on a non-first disk must identify the failing disk.
    ///
    /// Intent: When the single-passphrase invariant (Principle 4) is violated
    /// by external LUKS manipulation, the error message must name the specific
    /// disk that rejected the passphrase.
    ///
    /// Why it exists: Previously, ensure_luks_open failed with a generic
    /// "Wrong passphrase?" error — misleading because the passphrase had
    /// already been verified against another disk.
    ///
    /// Scenario: 2-disk RAID1 where someone ran `cryptsetup luksChangeKey` on
    /// disk2 outside of braid. `braid unlock` verifies against disk1
    /// (succeeds), opens disk1 (succeeds), then fails on disk2 with a message
    /// naming both disks.
    #[test]
    fn passphrase_mismatch_names_failing_disk() {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskConfig {
                    by_id: ByIdPath(path.to_owned()),
                },
            );
        }
        let config = Config::new(disks, MountPoint("/mnt/storage".to_owned())).unwrap();

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let runner = MockRunner::default()
            // mountpoint check → not mounted
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            // probe: disk1 is LUKS
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
            // probe: disk2 is LUKS
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
            // verify passphrase against disk1 → success
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open disk1 → success
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // open disk2 → FAILURE (different passphrase)
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                err_raw(
                    "cryptsetup open",
                    5,
                    "No key available with this passphrase.",
                ),
            );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let disk_map = DiskMap::new();
        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &disk_map,
            false,
            Some(tmp.path()),
            None,
            false,
        );

        let err = result.expect_err("should fail when disk2 rejects passphrase");
        let msg = err.to_string();
        assert!(
            msg.contains("disk2"),
            "error should name the failing disk, got: {msg}"
        );
        assert!(
            msg.contains("disk1"),
            "error should name the verification disk, got: {msg}"
        );
        assert!(
            !msg.contains("Wrong passphrase?"),
            "error should not say 'Wrong passphrase?', got: {msg}"
        );
    }
}
