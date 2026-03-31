use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{self, Config};
use crate::journal::{self, Journal};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::mount::{self, Credential, MountError};
use crate::parse::btrfs_filesystem_show::{classify_btrfs_probe, DeviceBtrfsProbe};
use crate::probe::{self, Filesystem, ProbeError};
use crate::state_paths::StatePaths;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoverError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("journal error: {0}")]
    Journal(String),
    #[error("membership error: {0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Mount(#[from] MountError),
    #[error("{0}")]
    Failed(String),
}

/// Rebuild pool.json from the live mounted pool and clear the pending-operation journal.
///
/// This is the only path out of recovery mode. It opens LUKS devices and mounts
/// the pool if needed, then probes the actual btrfs pool topology (not LUKS
/// labels) and builds membership from live state.
pub fn cmd_recover<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    paths: &StatePaths,
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    allow_degraded: bool,
) -> Result<(), RecoverError> {
    // 1. Load journal (required — nothing to recover if absent)
    let journal = match journal::load_journal(paths) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return Err(RecoverError::Failed(
                "no pending operation journal found — nothing to recover".into(),
            ));
        }
        Err(e) => return Err(RecoverError::Journal(e.to_string())),
    };

    eprintln!(
        "Recovering from interrupted {:?} operation (started {})...",
        journal_op_label(&journal),
        journal.started_at
    );

    // 2. Open LUKS devices and mount the pool if needed
    let union = union_memberships(&journal);
    let credential = Credential::Passphrase {
        passphrase_stdin,
        passphrase_file,
    };
    if let Err(e) = mount::open_and_mount_pool(
        runner,
        fs,
        config,
        &union,
        credential,
        allow_degraded,
        "recover",
    ) {
        // Bootstrap mount failure: probe the target devices to confirm no btrfs
        // superblock exists — only then is it safe to advise wiping.
        if journal.pre_membership.disks.is_empty() {
            if let mount::MountError::MountFailed(_) = &e {
                if let journal::OpKind::Add { ref disks } = journal.op {
                    let all_no_btrfs = disks.keys().all(|name| {
                        let mapper = format!("/dev/mapper/{}", config::mapper_name(name).0);
                        match runner.run(&CmdRequest::BtrfsFilesystemShowTarget { target: mapper })
                        {
                            Ok(raw) => {
                                matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs)
                            }
                            Err(_) => false,
                        }
                    });
                    if all_no_btrfs {
                        let disk_list: Vec<_> = union
                            .disks
                            .iter()
                            .map(|(name, m)| format!("  {} ({})", name, m.by_id))
                            .collect();
                        return Err(RecoverError::Failed(format!(
                            "bootstrap add was interrupted before the filesystem was \
                             created.\n\
                             The pool does not exist yet, so there is nothing to \
                             recover.\n\n\
                             To return to a clean state:\n\
                             1. rm {}\n\
                             2. Wipe the LUKS container from each disk that was being \
                                added:\n{}\n\
                                e.g.: wipefs -a /dev/disk/by-id/<device>\n\
                             3. Re-run braid add",
                            paths.pending_op_json().display(),
                            disk_list.join("\n"),
                        )));
                    }
                }
            }
        }
        return Err(e.into());
    }

    // 3. Probe live pool state
    let mount_point = config.mount_point().as_str();
    let pool = probe::probe_pool(runner, mount_point)?;

    // 4. Build new membership from live pool state
    let mut recovered = PoolMembership::empty();
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
            eprintln!("  skip: device {} has no braid- prefix", dev.mapper.0);
            continue;
        };
        // Get by_id from whichever membership snapshot knows about this device
        let by_id = union
            .disks
            .get(name)
            .map(|m| m.by_id.clone())
            .ok_or_else(|| {
                RecoverError::Failed(format!(
                    "device {} is in the live pool but has no by-id path in either \
                     the pre-operation or target membership snapshot.\n\
                     This must be resolved manually — provide the correct \
                     /dev/disk/by-id/ path and re-run recovery.",
                    dev.mapper.0
                ))
            })?;
        recovered.disks.insert(
            name.to_owned(),
            DiskMember::enriched(by_id, dev.luks_uuid.clone(), dev.devid),
        );
    }

    // 5. Report what changed
    let pre_names: std::collections::BTreeSet<_> = journal.pre_membership.disks.keys().collect();
    let target_names: std::collections::BTreeSet<_> =
        journal.target_membership.disks.keys().collect();
    let recovered_names: std::collections::BTreeSet<_> = recovered.disks.keys().collect();

    eprintln!("  pre-operation membership:  {:?}", pre_names);
    eprintln!("  target membership:         {:?}", target_names);
    eprintln!("  recovered (live pool):     {:?}", recovered_names);

    // 6. Write recovered membership
    membership::save_membership(&recovered, paths)?;
    eprintln!("pool.json written from live pool state.");

    // 7. Clear journal
    journal::clear_journal(paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");

    // Best-effort: warn if a paused balance was detected (e.g. crash during
    // RAID1 conversion). skip_balance prevents kernel auto-resume.
    crate::status::emit_paused_balance_warning(runner, mount_point, &mut std::io::stderr());

    Ok(())
}

fn journal_op_label(journal: &Journal) -> &'static str {
    match &journal.op {
        journal::OpKind::Add { .. } => "add",
        journal::OpKind::Remove { .. } => "remove",
        journal::OpKind::RemoveMissing { .. } => "remove-missing",
        journal::OpKind::Replace { .. } => "replace",
    }
}

/// Merge pre_membership and target_membership into a single set of all known devices.
fn union_memberships(journal: &Journal) -> PoolMembership {
    let mut union = journal.pre_membership.clone();
    for (name, member) in &journal.target_membership.disks {
        union
            .disks
            .entry(name.clone())
            .or_insert_with(|| member.clone());
    }
    union
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::journal::{self, OpKind};
    use crate::mount::MountError;
    use crate::probe::Filesystem;
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

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn ok_raw_empty(cmd: &str) -> RawCommandOutput {
        ok_raw(cmd, "")
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn mountpoint_ok() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".into()),
            },
            ok_raw_empty("mountpoint"),
        )
    }

    fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".into()),
            },
            err_raw("mountpoint", 1, ""),
        )
    }

    fn findmnt_btrfs() -> RawCommandOutput {
        ok_raw(
            "findmnt --json --output TARGET,SOURCE,FSTYPE -T /mnt/storage",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
        )
    }

    fn btrfs_show_toshiba_and_mystery() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-mystery\n",
        )
    }

    fn btrfs_show_two_disks() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n",
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

    /// Two-disk journal for interrupted add: pre has disk1+disk2, target has disk1+disk2+disk3.
    fn two_disk_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk3".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk3".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk3".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add { disks: add_disks },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// If a live pool device is absent from both the pre-operation and target
    /// membership snapshots, recovery must fail rather than fabricating a bogus
    /// by_id path. This protects against writing corrupt pool.json entries that
    /// would break subsequent unlock/lock cycles.
    ///
    /// Scenario: an interrupted add left a device in the btrfs pool that
    /// somehow appears in neither journal snapshot. Recovery should refuse to
    /// write pool.json and leave the journal intact so the user can intervene.
    #[test]
    fn recover_fails_when_device_missing_from_both_snapshots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        // pre and target both only know about "toshiba"
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "toshiba".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/ata-TOSHIBA".into())),
        );
        let pre = PoolMembership { disks: pre_disks };
        let target = pre.clone();

        // Op is adding "mystery" — but neither snapshot contains it
        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "mystery".to_owned(),
            ByIdPath("/dev/disk/by-id/ata-MYSTERY".into()),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add { disks: add_disks },
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&paths, &journal).unwrap();

        // Mock: pool is already mounted with both toshiba and mystery
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_toshiba_and_mystery(),
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
                    mapper: "braid-mystery".into(),
                },
                cryptsetup_status_active("braid-mystery", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let result = cmd_recover(&runner, &fs, &config, &paths, false, None, false);

        // Must fail with an error mentioning the unknown device
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("braid-mystery"),
            "error should name the unknown device, got: {msg}"
        );

        // pool.json must NOT have been written
        assert!(
            !paths.pool_json().exists(),
            "pool.json should not exist after failed recovery"
        );

        // pending-op.json must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist after failed recovery"
        );
    }

    /// Intent: When the pool is not mounted, recover should open LUKS devices,
    /// mount the pool, rebuild pool.json from live state, and clear the journal.
    ///
    /// Why: This is the core fix for the chicken-and-egg problem where unlock
    /// blocks on journal and recover blocks on unmounted pool.
    ///
    /// Scenario: 2-disk RAID1, interrupted add of disk3. Both disk1 and disk2
    /// are present with LUKS closed. Passphrase provided via file.
    #[test]
    fn recover_self_mounts_when_pool_not_mounted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            // mount helper: mountpoint check → not mounted
            .with_output(mp_req, mp_out)
            // mount helper: probe disk1 → LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // mount helper: probe disk2 → LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            // mount helper: verify passphrase against first disk
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // mount helper: open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // mount helper: open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // mount helper: btrfs device scan
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount helper: mount (disk3 absent → degraded)
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
            )
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &config,
            &paths,
            false,
            Some(passphrase_file.path()),
            true, // allow_degraded (disk3 is absent)
        );

        result.expect("recover should self-mount and succeed");

        // pool.json must have been written with disk1 and disk2
        assert!(paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(
            recovered.disks.contains_key("disk1"),
            "recovered membership should contain disk1"
        );
        assert!(
            recovered.disks.contains_key("disk2"),
            "recovered membership should contain disk2"
        );

        // pending-op.json must have been cleared
        assert!(
            !paths.pending_op_json().exists(),
            "journal should be cleared after recovery"
        );
    }

    /// Intent: When a disk is absent and --allow-degraded is not passed, recover
    /// must refuse with a structured DegradedRefused error.
    ///
    /// Why: Principle 1 requires explicit opt-in for degraded mounts, even
    /// during recovery.
    ///
    /// Scenario: 2-disk pool with interrupted add of disk3. disk3 is absent.
    /// allow_degraded=false.
    #[test]
    fn recover_refuses_degraded_without_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            );
        // No mount mock — should not reach mount

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &config,
            &paths,
            false,
            Some(passphrase_file.path()),
            false, // allow_degraded = false
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            matches!(&err, RecoverError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist after refused recovery"
        );
    }

    /// Intent: When the pool is already mounted, recover should skip the mount
    /// step and proceed directly to rebuilding pool.json.
    ///
    /// Why: The user may have manually opened LUKS and mounted, or the
    /// interrupted operation left the pool mounted. No passphrase needed.
    ///
    /// Scenario: 2-disk pool, already mounted. No passphrase mocks needed.
    #[test]
    fn recover_skips_mount_when_already_mounted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mount helper: mountpoint check → already mounted
            .with_output(mp_req, mp_out)
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        // No passphrase — pool is already mounted
        let result = cmd_recover(&runner, &fs, &config, &paths, false, None, false);

        result.expect("recover should succeed when pool already mounted");

        assert!(paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));
        assert!(
            !paths.pending_op_json().exists(),
            "journal should be cleared"
        );
    }

    /// Bootstrap journal: pre_membership is empty, target has one disk.
    fn bootstrap_journal() -> journal::Journal {
        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk1".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk1".into()),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add { disks: add_disks },
            pre_membership: PoolMembership::empty(),
            target_membership: target,
        }
    }

    /// Intent: when bootstrap add crashes after LUKS format but before mkfs,
    ///   recover detects the unmountable state and prints step-by-step escape
    ///   instructions.
    ///
    /// Why it exists: without this, the user is stuck in recovery mode with no
    ///   documented way out — recover fails, add is blocked by the journal, and
    ///   the error message gives no guidance.
    ///
    /// Scenario: first-ever braid add of one disk. LUKS format succeeded, crash
    ///   before mkfs.btrfs. User runs braid recover. Mount fails because no btrfs
    ///   superblock exists. Error should name the pending-op.json path, the disk's
    ///   by-id path, and wipefs.
    #[test]
    fn recover_bootstrap_crash_gives_actionable_instructions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe disk1 → PresentLuks
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // passphrase ok
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // LUKS open ok
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // btrfs scan ok
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount fails — no btrfs superblock
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                err_raw(
                    "mount",
                    32,
                    "wrong fs type, bad option, bad superblock on /dev/mapper/braid-disk1",
                ),
            )
            // btrfs probe confirms NoBtrfs
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                err_raw(
                    "btrfs filesystem show",
                    1,
                    "not a valid btrfs filesystem on /dev/mapper/braid-disk1",
                ),
            );

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &config,
            &paths,
            false,
            Some(passphrase_file.path()),
            false,
        );

        let err = result.expect_err("should fail with bootstrap instructions");
        let msg = err.to_string();
        assert!(
            msg.contains("bootstrap add was interrupted"),
            "expected bootstrap message, got: {msg}"
        );
        assert!(
            msg.contains("pending-op.json"),
            "should mention pending-op.json, got: {msg}"
        );
        assert!(msg.contains("wipefs"), "should mention wipefs, got: {msg}");
        assert!(
            msg.contains("virtio-disk1"),
            "should list disk by-id path, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
        // pool.json must NOT have been written
        assert!(!paths.pool_json().exists(), "pool.json should not exist");
    }

    /// Intent: when bootstrap recover fails due to wrong passphrase, the error
    ///   must be the original passphrase error — not the bootstrap escape
    ///   instructions.
    ///
    /// Why it exists: an earlier version caught all MountErrors during bootstrap
    ///   recovery, which would tell the user to wipe disks when the real problem
    ///   was just a typo in the passphrase.
    ///
    /// Scenario: first-ever braid add of one disk. LUKS format succeeded, crash
    ///   before mkfs. User runs braid recover with wrong passphrase. Error should
    ///   say "wrong passphrase", not "bootstrap add was interrupted".
    #[test]
    fn recover_bootstrap_wrong_passphrase_not_masked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe disk1 → PresentLuks
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // passphrase FAILS
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"wrongpass".to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            );

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"wrongpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &config,
            &paths,
            false,
            Some(passphrase_file.path()),
            false,
        );

        let err = result.expect_err("should fail with passphrase error");
        let msg = err.to_string();
        assert!(
            msg.contains("wrong passphrase"),
            "expected passphrase error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap add was interrupted"),
            "must not show bootstrap message for passphrase error, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }

    /// Intent: when a non-bootstrap recover hits a mount failure, the original
    ///   mount error propagates without bootstrap rewriting.
    ///
    /// Why it exists: the bootstrap detection must key off pre_membership being
    ///   empty. A non-empty pre_membership with a mount failure is a different
    ///   situation (e.g. damaged pool) that needs the real error.
    ///
    /// Scenario: 2-disk pool, interrupted add of disk3. All three disks absent.
    ///   Error should be the original "no unlockable disks", not bootstrap advice.
    #[test]
    fn recover_non_bootstrap_mount_failure_propagates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]); // all disks absent

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);

        let result = cmd_recover(&runner, &fs, &config, &paths, false, None, false);

        let err = result.expect_err("should fail with mount error");
        let msg = err.to_string();
        assert!(
            msg.contains("no unlockable disks"),
            "expected original mount error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap"),
            "must not show bootstrap message for non-bootstrap case, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }

    /// Intent: when bootstrap recover's mount fails but the disk actually has a
    ///   btrfs superblock, the original mount error must propagate — the guidance
    ///   to wipe disks would be wrong.
    ///
    /// Why it exists: mkfs may have succeeded but mount failed for another reason
    ///   (missing kernel module, bad options). Telling the user to wipefs would
    ///   destroy a valid filesystem.
    ///
    /// Scenario: first-ever add of one disk. mkfs.btrfs succeeded, mount failed
    ///   for an unrelated reason. btrfs filesystem show confirms HasBtrfs. Error
    ///   should be the original mount error, not bootstrap guidance.
    #[test]
    fn recover_bootstrap_mount_fails_but_btrfs_exists_propagates_mount_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe disk1 → PresentLuks
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // passphrase ok
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // LUKS open ok
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // btrfs scan ok
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount fails for non-btrfs reason
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                err_raw(
                    "mount",
                    32,
                    "mount(2) system call failed: Permission denied",
                ),
            )
            // btrfs probe confirms HasBtrfs — mkfs DID succeed
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 256.00KiB\n\
                     \tdevid    1 size 10.00GiB used 536.00MiB path /dev/mapper/braid-disk1\n",
                ),
            );

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &config,
            &paths,
            false,
            Some(passphrase_file.path()),
            false,
        );

        let err = result.expect_err("should fail with original mount error");
        let msg = err.to_string();
        assert!(
            msg.contains("mount failed"),
            "expected original mount error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap add was interrupted"),
            "must not show bootstrap message when btrfs exists, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }
}
