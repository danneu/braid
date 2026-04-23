use crate::cmd::{CommandRunner, Step};
use crate::config::Config;
use crate::membership::{self, PoolMembership};
use crate::mount::{self, MountError};
use crate::preflight;
use crate::probe::{self, Filesystem};
use crate::state_paths::StatePaths;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{0}")]
    Mount(#[from] MountError),
    #[error("{0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Failed(String),
}

pub struct UnlockParams<'a> {
    pub config: &'a Config,
    pub membership: &'a PoolMembership,
    pub paths: &'a StatePaths,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub key_file: Option<&'a Path>,
    pub allow_degraded: bool,
    pub dry_run: bool,
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &UnlockParams<'_>,
) -> Result<(), UnlockError> {
    preflight::check_no_pending_operation(params.paths).map_err(UnlockError::Failed)?;

    // Plan once (read-only). Reused by dry-run and execution.
    let report = mount::plan_open_pool(
        runner,
        fs,
        params.config,
        params.membership,
        params.allow_degraded,
        "unlock",
    );
    mount::print_probe_events(&report.events);
    let plan = report.result?;

    // Dry-run: print steps and exit. plan_open_pool already validated.
    if params.dry_run {
        if let Some(ref p) = plan {
            let steps = mount::compile_open_steps(p, params.config.mount_point(), params.key_file);
            Step::print_dry_run(&steps);
        }
        return Ok(());
    }

    // Contract:
    // - Pure operator command: bring the pool online from authoritative state.
    // - Membership comes from pool.json; unlock never creates, repairs, or rewrites it.
    // - Probe only configured members, open what is available, and mount the pool.
    // - Refuse degraded mounts unless --allow-degraded is explicit.
    // - After a successful mount, pool.json enriched fields (luks_uuid, devid) are
    //   refreshed best-effort, but correctness never depends on that write.

    let Some(plan) = plan else {
        // Pool was already mounted (plan_open_pool returned None).
        return Ok(());
    };

    // Unlock-specific gate: only resolve a credential if there is something
    // to unlock. Preserves the "no prompt when every mapper is already open"
    // UX rule that used to live inside open_and_mount_pool.
    if plan.to_unlock.is_empty() {
        mount::execute_mount_only(runner, fs, params.config, &plan)?;
    } else {
        let credential = mount::resolve_credential(
            params.passphrase_stdin,
            params.passphrase_file,
            params.key_file,
        )?;
        mount::execute_unlock_and_mount(runner, fs, params.config, &plan, &credential)?;
    }

    let mount_point = params.config.mount_point();

    // Enrich pool.json with live metadata (luks_uuid, devid) -- best-effort.
    // A rare race where probe_pool sees mounted=false after a successful
    // mount leaves `pool_after.devices` empty, so refresh_pool_metadata
    // no-ops. That is acceptable: correctness never depends on this write
    // (see contract above). Pinned by
    // unlock_tolerates_post_mount_probe_mounted_false.
    if let Ok(pool_after) = probe::probe_pool(runner, mount_point) {
        membership::refresh_pool_metadata(&pool_after, params.paths);
    }

    // Best-effort: warn if a paused balance was found on mount.
    // skip_balance prevents the kernel from resuming it silently, but the user
    // should know so they can resume or cancel explicitly.
    crate::status::emit_paused_balance_warning(runner, mount_point, &mut std::io::stderr());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::mount::MountError;
    use crate::probe::Filesystem;
    use crate::state_paths::StatePaths;
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::BTreeMap;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

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

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
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
        let (_state_dir, sp) = test_paths();
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
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        // Write passphrase to a temp file for the test (avoid stdin TTY)
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: true,
                dry_run: false,
            },
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
        let (_state_dir, sp) = test_paths();
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
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        // No mount mock — should never reach mount

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
            },
        );

        let err = result.expect_err("should refuse degraded mount without --allow-degraded");
        assert!(
            matches!(&err, UnlockError::Mount(MountError::DegradedRefused(_))),
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
        let (_state_dir, sp) = test_paths();
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
                    2,
                    "No key available with this passphrase.",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
            },
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
        let (_state_dir, sp) = test_paths();
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
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
            },
        );

        // The paused balance warning must not cause unlock to fail.
        result.expect("unlock should succeed even with paused balance");
    }

    // Intent: dry-run for unlock with 2 closed disks shows LUKS open + scan + mount.
    // Why: verifies plan_open_pool + compile_open_steps integration via cmd_unlock.
    // Scenario: 2-disk pool, both present, both closed, --dry-run.
    #[test]
    fn dry_run_render_unlock_2_closed_disks() {
        let (_state_dir, sp) = test_paths();
        let config = Config::new(MountPoint("/mnt/storage".to_owned())).unwrap();
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        let membership = PoolMembership { disks };
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
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
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        // dry_run = true, no passphrase needed
        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: None,
                key_file: None,
                allow_degraded: false,
                dry_run: true,
            },
        );
        result.expect("dry-run unlock should succeed");
    }

    // Intent: dry-run for unlock with degraded refusal returns the same error.
    // Why: dry-run must run the same validation as execution.
    // Scenario: 3-disk pool, disk3 absent, --dry-run without --allow-degraded.
    #[test]
    fn dry_run_unlock_degraded_refused() {
        let (_state_dir, sp) = test_paths();
        let config = three_disk_config();
        let membership = three_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
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
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: None,
                key_file: None,
                allow_degraded: false,
                dry_run: true,
            },
        );

        let err = result.expect_err("dry-run should refuse degraded mount");
        assert!(
            matches!(&err, UnlockError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}"
        );
    }

    /// Intent: `cmd_unlock` must tolerate a post-mount `probe_pool` that
    /// returns `Ok(PoolState { mounted: false, devices: vec![], ... })`
    /// without enriching pool.json and without failing.
    ///
    /// Why it exists: the post-mount enrichment at `cli/src/unlock.rs:94`
    /// is best-effort. If `findmnt` races the mount (exit 1, empty stderr)
    /// `probe_pool` returns `Ok(mounted=false)` instead of an error, which
    /// still enters the `if let Ok(...)` arm. Without a pinning test,
    /// nothing prevents a future refactor from propagating that `Ok` as a
    /// hard failure or from silently enriching with stale data.
    ///
    /// Scenario: 3-disk pool, clean mount. The post-mount `findmnt` call
    /// happens to exit 1 with empty stderr (benign miss). pool.json was
    /// seeded with `luks_uuid=None`/`devid=None`; those fields must remain
    /// `None` after `cmd_unlock` returns `Ok(())`, proving no enrichment
    /// occurred.
    #[test]
    fn unlock_tolerates_post_mount_probe_mounted_false() {
        let (_state_dir, sp) = test_paths();
        let config = three_disk_config();
        let membership = three_disk_membership();
        // Seed pool.json with None metadata so we can assert non-enrichment.
        membership::save_membership(&membership, &sp)
            .expect("seed pool.json for assertion baseline");

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let runner = MockRunner::default()
            // mountpoint check -> not mounted
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
            // post-mount probe_pool: findmnt returns exit 1 + empty stderr.
            // Per parse_findmnt_json, that yields FindmntOutput with no
            // filesystems, which probe_pool converts to
            // Ok(PoolState { mounted: false, devices: vec![], ... }).
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("findmnt", 1, ""),
            )
            // balance status -> no balance (post-enrichment warn step)
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
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
            },
        );

        result.expect("unlock should tolerate probe_pool returning mounted=false");

        let loaded = membership::load_membership(&sp)
            .expect("pool.json should still be loadable after unlock");
        for name in ["disk1", "disk2", "disk3"] {
            let member = loaded
                .disks
                .get(name)
                .unwrap_or_else(|| panic!("missing disk {name} in pool.json"));
            assert!(
                member.luks_uuid.is_none(),
                "{name}.luks_uuid must remain None when probe_pool returns \
                 mounted=false, got: {:?}",
                member.luks_uuid
            );
            assert!(
                member.devid.is_none(),
                "{name}.devid must remain None when probe_pool returns \
                 mounted=false, got: {:?}",
                member.devid
            );
        }
    }

    /// Intent: When every membership disk's mapper is already open,
    /// `cmd_unlock` must take the mount-only branch and NEVER call
    /// `resolve_credential` -- preserving the UX rule that a redundant
    /// unlock does not prompt for or read a passphrase.
    ///
    /// Why it exists: This is the cross-file counterpart to
    /// `execute_mount_only_rejects_non_empty_plan`/`execute_unlock_and_mount_rejects_empty_plan`
    /// in mount.rs. Those two tests pin the entry-point contracts; this
    /// test pins the caller-side dispatch. A natural-looking refactor
    /// (hoisting `resolve_credential` above the `plan.to_unlock.is_empty()`
    /// branch) would break the no-prompt UX and also start erroring on
    /// operators who don't supply credentials for an already-open pool.
    /// Without this test, that regression passes `mount.rs`'s boundary
    /// tests because they bypass `cmd_unlock`'s dispatch entirely.
    ///
    /// Scenario: 3-disk pool, all mappers already open at probe time. The
    /// `UnlockParams` point `passphrase_file` at a path that does not
    /// exist -- if `resolve_credential` is called, `read_passphrase` would
    /// fail with a "failed to read passphrase file" error and surface
    /// through `cmd_unlock`. A clean `Ok(())` proves the mount-only
    /// branch was taken.
    #[test]
    fn cmd_unlock_skips_credential_resolution_when_nothing_to_unlock() {
        let (_state_dir, sp) = test_paths();
        let config = three_disk_config();
        let membership = three_disk_membership();

        // All three by-id devices AND all three mapper paths exist.
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-disk3",
        ]);

        let runner = MockRunner::default()
            // Pool not mounted yet.
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            // probe: each disk reports its LUKS UUID.
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
            // All mappers already open with matching backing-device UUIDs.
            .with_mapper_open(
                "braid-disk1",
                "/dev/vda",
                "aaaaaaaa-1111-2222-3333-444444444444",
            )
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "bbbbbbbb-1111-2222-3333-444444444444",
            )
            .with_mapper_open(
                "braid-disk3",
                "/dev/vdc",
                "cccccccc-1111-2222-3333-444444444444",
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ])
            // No CryptsetupTestPassphrase / CryptsetupLuksOpen mocks -- if
            // the code takes the unlock-and-mount branch, these lookups
            // miss and MockRunner fails the test.
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            )
            // post-mount probe_pool: benign miss, keeps test focused.
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("findmnt", 1, ""),
            )
            // balance status post-mount
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

        // passphrase_file points at a path that does not exist. If the
        // dispatch regresses and hoists resolve_credential above the
        // `plan.to_unlock.is_empty()` check, read_passphrase will fail
        // with a "failed to read passphrase file" error and this test
        // will no longer reach Ok(()).
        let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");

        let result = cmd_unlock(
            &runner,
            &fs,
            &UnlockParams {
                config: &config,
                membership: &membership,
                paths: &sp,
                passphrase_stdin: false,
                passphrase_file: Some(&bogus),
                key_file: None,
                allow_degraded: false,
                dry_run: false,
            },
        );

        result.expect(
            "unlock with all mappers already open must take the mount-only \
             branch and never attempt to read the (nonexistent) passphrase file",
        );
    }
}
