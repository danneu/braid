use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, Config};
use crate::luks::{self, LuksError};
use crate::membership::{self, PoolMembership};
use crate::pool::PoolError;
use crate::preflight;
use crate::probe::{self, Filesystem, ProbeError};
use crate::state_paths::StatePaths;
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
    Membership(#[from] membership::MembershipError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    DegradedRefused(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    paths: &StatePaths,
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    key_file: Option<&std::path::Path>,
    allow_degraded: bool,
) -> Result<(), UnlockError> {
    preflight::check_no_pending_operation(paths).map_err(UnlockError::Failed)?;

    // Contract:
    // - Pure operator command: bring the pool online from authoritative state.
    // - Membership comes from pool.json; unlock never creates, repairs, or rewrites it.
    // - Probe only configured members, open what is available, and mount the pool.
    // - Refuse degraded mounts unless --allow-degraded is explicit.
    // - After a successful mount, pool.json enriched fields (luks_uuid, devid) are
    //   refreshed best-effort, but correctness never depends on that write.
    let mount_point = config.mount_point();

    // 1. If pool already mounted → print message, exit 0
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(());
    }

    // 2. Probe each membership disk
    let mut to_unlock = Vec::new(); // (name, by_id) pairs needing unlock
    let mut any_open = false;
    let mut any_missing_member = false;

    for (name, member) in &membership.disks {
        let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                // Known pool member, confirmed missing → degradable
                eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
                any_missing_member = true;
            }
            ConfigDiskState::PresentNotLuks => {
                // Was a pool member, LUKS header now bricked → degradable
                eprintln!("{}  disk: {:<10}LUKS header damaged", tag("skip"), name);
                any_missing_member = true;
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
                to_unlock.push((name.clone(), member.by_id.clone()));
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        return Err(UnlockError::Failed("no unlockable disks found".into()));
    }

    // 4. If disks need opening → verify credential, then open each disk
    if !to_unlock.is_empty() {
        if let Some(kf) = key_file {
            // Keyfile path: verify against first disk, then open each
            let (ref first_name, ref first_by_id) = to_unlock[0];
            let ok = luks::verify_key_file(runner, &first_by_id.0, kf)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong keyfile (verified against {})",
                    first_name
                )));
            }

            for (name, by_id) in &to_unlock {
                luks::ensure_luks_open_with_key_file(runner, fs, name, by_id, kf).map_err(
                    |_| {
                        UnlockError::Failed(format!(
                            "failed to open disk '{}': keyfile was verified against \
                             '{}' but rejected here (single-passphrase invariant \
                             may be violated by external LUKS manipulation)",
                            name, first_name
                        ))
                    },
                )?;
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
            }
        } else {
            // Passphrase path
            let passphrase = luks::read_passphrase(passphrase_file, passphrase_stdin)?;

            let (ref first_name, ref first_by_id) = to_unlock[0];
            let ok = luks::verify_passphrase(runner, &first_by_id.0, &passphrase)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong passphrase (verified against {})",
                    first_name
                )));
            }

            for (name, by_id) in &to_unlock {
                luks::ensure_luks_open(runner, fs, name, by_id, &passphrase).map_err(|_| {
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
        .or_else(|| membership.disks.keys().next().map(|k| k.as_str()))
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

    // Enrich pool.json with live metadata (luks_uuid, devid) — best-effort.
    if let Ok(pool_after) = probe::probe_pool(runner, mount_point.as_str()) {
        membership::refresh_pool_metadata(&pool_after, paths);
    }

    // Best-effort: warn if a paused balance was found on mount.
    // skip_balance prevents the kernel from resuming it silently, but the user
    // should know so they can resume or cancel explicitly.
    match crate::status::get_balance_report(runner, mount_point.as_str()) {
        crate::status::BalanceReport::Paused { .. } => {
            eprintln!(
                "{}  {:<10}paused balance detected \u{2014} will not auto-resume",
                tag("warn"),
                ""
            );
            eprintln!("           resume:  btrfs balance resume {mount_point}");
            eprintln!("           cancel:  btrfs balance cancel {mount_point}");
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::state_paths::StatePaths;
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

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
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
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn three_disk_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
            ("disk3", "/dev/disk/by-id/virtio-disk3"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        PoolMembership { disks }
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
        let membership = three_disk_membership();

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
                ok_raw("mount -o noatime,skip_balance,degraded"),
            )
            // 7. balance status check after mount (best-effort)
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
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
            &membership,
            &StatePaths::production(),
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
        let membership = three_disk_membership();

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
            &membership,
            &StatePaths::production(),
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
        let config = Config::new(MountPoint("/mnt/storage".to_owned())).unwrap();
        let mut membership_disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ] {
            membership_disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        let membership = PoolMembership {
            disks: membership_disks,
        };

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

        let result = cmd_unlock(
            &runner,
            &fs,
            &config,
            &membership,
            &StatePaths::production(),
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

    /// Paused balance after unlock succeeds (warning is informational only).
    ///
    /// Intent: When a paused balance is detected after mount, unlock must still
    /// return Ok(()) — the warning is informational, not an error.
    ///
    /// Why it exists: The post-mount balance check must not accidentally convert
    /// an informational warning into a failure that breaks auto-unlock.
    ///
    /// Scenario: 3-disk RAID1, all healthy. A balance was paused before lock.
    /// On re-unlock, skip_balance prevents kernel auto-resume, and the CLI
    /// prints a warning. Unlock still succeeds.
    #[test]
    fn unlock_warns_on_paused_balance() {
        let config = three_disk_config();
        let membership = three_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default()
            // mountpoint check → not mounted
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            // probe: all 3 disks are LUKS
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
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: "cccccccc-1111-2222-3333-444444444444\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            // verify passphrase
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open all 3 disks
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                    mapper: "braid-disk3".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            // btrfs device scan
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            // normal mount (all present)
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount -o noatime,skip_balance"),
            )
            // balance status → PAUSED
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status".into(),
                    stdout: "Balance on '/mnt/storage' is paused\n\
                             3 out of about 10 chunks balanced (7 considered), \
                             70% left\n"
                        .into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
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
            &membership,
            &StatePaths::production(),
            false,
            Some(tmp.path()),
            None,
            false,
        );

        // The paused balance warning must not cause unlock to fail.
        result.expect("unlock should succeed even with paused balance");
    }
}
