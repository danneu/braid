use crate::cmd::CommandRunner;
use crate::config::{self, Config};
use crate::journal::{self, Journal};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::{self, ProbeError};
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
    Failed(String),
}

/// Rebuild pool.json from the live mounted pool and clear the pending-operation journal.
///
/// This is the only path out of recovery mode. It probes the actual btrfs pool
/// topology (not LUKS labels) and builds membership from live state.
pub fn cmd_recover<R: CommandRunner>(
    runner: &R,
    config: &Config,
    paths: &StatePaths,
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

    // 2. Pool must be mounted — recover rebuilds from live state
    let mount_point = config.mount_point().as_str();
    let pool = match probe::probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(_) => {
            return Err(RecoverError::Failed(format!(
                "pool is not mounted at {}. To recover:\n\
                 1. Manually open LUKS devices: cryptsetup open /dev/disk/by-id/... braid-<name>\n\
                 2. Mount the pool: mount /dev/mapper/braid-<name> {}\n\
                 3. Run 'braid recover' again",
                mount_point, mount_point,
            )));
        }
    };

    // 3. Build new membership from live pool state
    let mut recovered = PoolMembership::empty();
    let union = union_memberships(&journal);
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

    // 4. Report what changed
    let pre_names: std::collections::BTreeSet<_> = journal.pre_membership.disks.keys().collect();
    let target_names: std::collections::BTreeSet<_> =
        journal.target_membership.disks.keys().collect();
    let recovered_names: std::collections::BTreeSet<_> = recovered.disks.keys().collect();

    eprintln!("  pre-operation membership:  {:?}", pre_names);
    eprintln!("  target membership:         {:?}", target_names);
    eprintln!("  recovered (live pool):     {:?}", recovered_names);

    // 5. Write recovered membership
    membership::save_membership(&recovered, paths)?;
    eprintln!("pool.json written from live pool state.");

    // 6. Clear journal
    journal::clear_journal(paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");

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
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::BTreeMap;

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
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

        // Mock: pool is mounted with both toshiba and mystery
        let runner = MockRunner::default()
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

        let result = cmd_recover(&runner, &config, &paths);

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
}
