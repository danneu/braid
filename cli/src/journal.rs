use crate::membership::PoolMembership;
use crate::state_io::atomic_write;
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, LuksUuid, MapperName};
use crate::util::now_iso;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// A pending-operation journal records the full context of a mutation in progress.
/// When this file exists, braid enters recovery mode: only `status`, `recover`,
/// and `lock` are permitted. All other commands hard-fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub started_at: String,
    pub op: OpKind,
    /// Snapshot of pool.json at journal write time — known-good state before the mutation.
    pub pre_membership: PoolMembership,
    /// What pool.json should become if the mutation succeeds.
    pub target_membership: PoolMembership,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AddPhase {
    PoolMutation,
    PostAddBalanceRaid1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddJournalTarget {
    pub by_id: ByIdPath,
    pub mapper_name: String,
    pub mode: AddJournalMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AddJournalMode {
    RecoverableBraidLabeled {
        verified_pool_fsid: String,
        luks_uuid: LuksUuid,
        /// Keyfile to enroll into LUKS slot 1 if `add --enroll DIR` was
        /// passed against this returning braid disk and slot 1 was empty
        /// at planning time. `None` covers both the no-`--enroll` case
        /// and the idempotent-skip case (slot 1 already authenticates).
        /// Recovery replays `cryptsetup luksAddKey` + `luksHeaderBackup`
        /// before pool_add_device when this is `Some`.
        enroll_key_file: Option<PathBuf>,
    },
    FreshLuks {
        luks_label: String,
        luks_format_extra_opts: Vec<String>,
        enroll_key_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoveMissingPhase {
    PoolMutation,
    PostRemoveMissingMaintenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplacePhase {
    PoolMutation,
    PostReplaceMaintenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplaceJournalSource {
    Live {
        old_devid: u64,
        old_mapper: MapperName,
    },
    Missing {
        old_devid: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplaceJournalTarget {
    pub by_id: ByIdPath,
    pub mapper_name: String,
    pub mode: ReplaceJournalMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplaceJournalMode {
    FreshLuks {
        luks_label: String,
        luks_format_extra_opts: Vec<String>,
        enroll_key_file: Option<PathBuf>,
    },
    ExistingLuks {
        luks_uuid: LuksUuid,
        /// Keyfile to enroll into LUKS slot 1 if `replace --enroll DIR`
        /// was passed against an already-formatted new disk and slot 1
        /// was empty at planning time. `None` covers both no-`--enroll`
        /// and the idempotent-skip case. Recovery replays
        /// `cryptsetup luksAddKey` + `luksHeaderBackup` after the LUKS
        /// UUID identity probe and credential verification when `Some`.
        enroll_key_file: Option<PathBuf>,
    },
}

// One in-flight OpKind per CLI invocation; boxing the ~200-byte ReplaceJournalTarget variant would litter every match arm and serde derive without measurable benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum OpKind {
    Add {
        phase: AddPhase,
        targets: BTreeMap<String, AddJournalTarget>,
    },
    Remove {
        name: String,
    },
    RemoveMissing {
        phase: RemoveMissingPhase,
        devid: u64,
        restore_raid1_after_commit: bool,
    },
    Replace {
        phase: ReplacePhase,
        old_name: String,
        new_name: String,
        new_target: ReplaceJournalTarget,
        source: ReplaceJournalSource,
        restore_raid1_after_commit: bool,
    },
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("failed to write journal: {0}")]
    Write(#[source] std::io::Error),
    #[error("failed to read journal: {0}")]
    Read(#[source] std::io::Error),
    #[error("failed to parse journal: {0}")]
    Parse(String),
    #[error("failed to delete journal: {0}")]
    Delete(#[source] std::io::Error),
    #[error("failed to serialize journal: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// Write the pending operation journal atomically.
pub fn write_journal(paths: &StatePaths, journal: &Journal) -> Result<(), JournalError> {
    let json = serde_json::to_string_pretty(journal).map_err(JournalError::Serialize)?;
    atomic_write(&paths.pending_op_json(), json.as_bytes()).map_err(JournalError::Write)
}

/// Load journal if present. Returns None if file doesn't exist.
pub fn load_journal(paths: &StatePaths) -> Result<Option<Journal>, JournalError> {
    let path = paths.pending_op_json();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(JournalError::Read(e)),
    };
    let journal: Journal =
        serde_json::from_str(&contents).map_err(|e| JournalError::Parse(e.to_string()))?;
    Ok(Some(journal))
}

/// Durably delete the journal file. Fsyncs the parent directory after removal
/// so the deletion survives power loss. Returns Ok if file doesn't exist.
pub fn clear_journal(paths: &StatePaths) -> Result<(), JournalError> {
    let path = paths.pending_op_json();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let dir = path.parent().ok_or_else(|| {
                JournalError::Delete(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "journal path has no parent directory",
                ))
            })?;
            crate::state_io::sync_dir(dir).map_err(JournalError::Delete)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(JournalError::Delete(e)),
    }
}

/// Build a journal for a mutation. Snapshots current membership as pre_membership,
/// accepts the computed target_membership, and records the operation kind.
pub fn build_journal(
    pre_membership: PoolMembership,
    target_membership: PoolMembership,
    op: OpKind,
) -> Journal {
    Journal {
        started_at: now_iso(),
        op,
        pre_membership,
        target_membership,
    }
}

/// Rewrite the pending-operation journal after a durable phase transition.
///
/// The caller supplies the next op shape and may replace target_membership
/// with a committed, freshly-enriched membership snapshot.
pub fn rewrite_journal(
    paths: &StatePaths,
    journal: &Journal,
    op: OpKind,
    target_membership: Option<PoolMembership>,
) -> Result<Journal, JournalError> {
    let mut next = journal.clone();
    next.op = op;
    if let Some(target_membership) = target_membership {
        next.target_membership = target_membership;
    }
    write_journal(paths, &next)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::state_paths::StatePaths;
    use crate::types::ByIdPath;

    fn sample_membership() -> PoolMembership {
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/ata-X".into())),
        );
        m
    }

    fn sample_journal() -> Journal {
        build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Add {
                phase: AddPhase::PoolMutation,
                targets: BTreeMap::from([(
                    "disk2".into(),
                    AddJournalTarget {
                        by_id: ByIdPath("/dev/disk/by-id/ata-Y".into()),
                        mapper_name: "braid-disk2".into(),
                        mode: AddJournalMode::RecoverableBraidLabeled {
                            verified_pool_fsid: "fsid-1".into(),
                            luks_uuid: LuksUuid("luks-1".into()),
                            enroll_key_file: None,
                        },
                    },
                )]),
            },
        )
    }

    #[test]
    fn write_and_load_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        let journal = sample_journal();
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().expect("should be Some");
        assert_eq!(loaded.op, journal.op);
        assert_eq!(loaded.pre_membership, journal.pre_membership);
        assert_eq!(loaded.target_membership, journal.target_membership);
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        assert!(load_journal(&paths).unwrap().is_none());
    }

    #[test]
    fn clear_removes_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        write_journal(&paths, &sample_journal()).unwrap();
        clear_journal(&paths).unwrap();
        assert!(load_journal(&paths).unwrap().is_none());
    }

    /// Intent: clear_journal durably deletes the file by fsyncing the parent
    /// directory. This test confirms the full durable-delete path executes
    /// without error and the file is actually gone.
    ///
    /// Why it exists: clear_journal previously used a bare remove_file without
    /// dir fsync, risking journal reappearance after power loss.
    ///
    /// Scenario: a mutation succeeds, pool.json is written, and the journal is
    /// cleared. If the system crashes immediately after, the journal must not
    /// reappear on reboot.
    #[test]
    fn clear_journal_fsyncs_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        write_journal(&paths, &sample_journal()).unwrap();
        assert!(paths.pending_op_json().exists());
        clear_journal(&paths).unwrap();
        assert!(!paths.pending_op_json().exists());
    }

    #[test]
    fn clear_missing_file_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        clear_journal(&paths).unwrap();
    }

    #[test]
    fn load_corrupt_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::write(paths.pending_op_json(), "not json").unwrap();
        assert!(matches!(load_journal(&paths), Err(JournalError::Parse(_))));
    }

    #[test]
    fn roundtrip_remove_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::Remove {
                name: "disk1".into(),
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    #[test]
    fn roundtrip_replace_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Replace {
                phase: ReplacePhase::PoolMutation,
                old_name: "disk1".into(),
                new_name: "disk2".into(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath("/dev/disk/by-id/ata-NEW".into()),
                    mapper_name: "braid-disk2".into(),
                    mode: ReplaceJournalMode::ExistingLuks {
                        luks_uuid: LuksUuid("luks-new".into()),
                        enroll_key_file: None,
                    },
                },
                source: ReplaceJournalSource::Live {
                    old_devid: 1,
                    old_mapper: MapperName("braid-disk1".into()),
                },
                restore_raid1_after_commit: false,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let serialized: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(paths.pending_op_json()).unwrap())
                .unwrap();
        let op = serialized.get("op").unwrap();
        assert!(
            op.get("new_by_id").is_none(),
            "replace journal must not duplicate new target by-id at op root"
        );
        assert_eq!(
            op.pointer("/new_target/by_id")
                .and_then(|value| value.as_str()),
            Some("/dev/disk/by-id/ata-NEW")
        );
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    /// Roundtrip the `Some(kf)` shape of `ReplaceJournalMode::ExistingLuks`.
    /// Why it exists: this PR widens the variant with `enroll_key_file:
    /// Option<PathBuf>` so `replace --enroll DIR` against an already-LUKS
    /// new disk journals the keyfile for crash-recovery replay. Catching
    /// a serde drift on the populated arm specifically (not just `None`)
    /// keeps the recovery contract observable.
    #[test]
    fn roundtrip_replace_existing_luks_with_enroll_key_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Replace {
                phase: ReplacePhase::PoolMutation,
                old_name: "disk1".into(),
                new_name: "disk2".into(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath("/dev/disk/by-id/ata-NEW".into()),
                    mapper_name: "braid-disk2".into(),
                    mode: ReplaceJournalMode::ExistingLuks {
                        luks_uuid: LuksUuid("luks-new".into()),
                        enroll_key_file: Some(PathBuf::from("/run/keys/braid.key")),
                    },
                },
                source: ReplaceJournalSource::Live {
                    old_devid: 1,
                    old_mapper: MapperName("braid-disk1".into()),
                },
                restore_raid1_after_commit: false,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    /// Roundtrip the `Some(kf)` shape of
    /// `AddJournalMode::RecoverableBraidLabeled`. Same rationale as
    /// `roundtrip_replace_existing_luks_with_enroll_key_file`: this PR
    /// extends the existing recoverable-add variant with `enroll_key_file`
    /// so `add --enroll DIR` against an `OpenRecoverable` /
    /// `ClosedPresentLuks` target journals the keyfile for replay.
    #[test]
    fn roundtrip_add_recoverable_with_enroll_key_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Add {
                phase: AddPhase::PoolMutation,
                targets: BTreeMap::from([(
                    "disk2".into(),
                    AddJournalTarget {
                        by_id: ByIdPath("/dev/disk/by-id/ata-Y".into()),
                        mapper_name: "braid-disk2".into(),
                        mode: AddJournalMode::RecoverableBraidLabeled {
                            verified_pool_fsid: "fsid-1".into(),
                            luks_uuid: LuksUuid("luks-1".into()),
                            enroll_key_file: Some(PathBuf::from("/run/keys/braid.key")),
                        },
                    },
                )]),
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    #[test]
    fn roundtrip_remove_missing_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::RemoveMissing {
                phase: RemoveMissingPhase::PoolMutation,
                devid: 3,
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    #[test]
    fn roundtrip_add_post_balance_fresh_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Add {
                phase: AddPhase::PostAddBalanceRaid1,
                targets: BTreeMap::from([(
                    "disk2".into(),
                    AddJournalTarget {
                        by_id: ByIdPath("/dev/disk/by-id/ata-Y".into()),
                        mapper_name: "braid-disk2".into(),
                        mode: AddJournalMode::FreshLuks {
                            luks_label: "braid-disk2".into(),
                            luks_format_extra_opts: vec![
                                "--perf-no_read_workqueue".into(),
                                "--label".into(),
                                "braid-disk2".into(),
                            ],
                            enroll_key_file: Some(PathBuf::from("/run/keys/braid.key")),
                        },
                    },
                )]),
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }

    #[test]
    fn rewrite_journal_preserves_context_and_replaces_op() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = sample_journal();
        write_journal(&paths, &journal).unwrap();

        let mut committed = sample_membership();
        committed.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/ata-Y".into())),
        );
        let next = rewrite_journal(
            &paths,
            &journal,
            OpKind::Add {
                phase: AddPhase::PostAddBalanceRaid1,
                targets: match &journal.op {
                    OpKind::Add { targets, .. } => targets.clone(),
                    _ => unreachable!(),
                },
            },
            Some(committed.clone()),
        )
        .unwrap();

        assert_eq!(next.started_at, journal.started_at);
        assert_eq!(next.pre_membership, journal.pre_membership);
        assert_eq!(next.target_membership, committed);
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, next);
    }

    #[test]
    fn rewrite_journal_preserves_context_for_replace_phase() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Replace {
                phase: ReplacePhase::PoolMutation,
                old_name: "disk1".into(),
                new_name: "disk2".into(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath("/dev/disk/by-id/ata-Y".into()),
                    mapper_name: "braid-disk2".into(),
                    mode: ReplaceJournalMode::ExistingLuks {
                        luks_uuid: LuksUuid("luks-2".into()),
                        enroll_key_file: None,
                    },
                },
                source: ReplaceJournalSource::Live {
                    old_devid: 1,
                    old_mapper: MapperName("braid-disk1".into()),
                },
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let mut committed = sample_membership();
        committed.disks.remove("disk1");
        committed.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/ata-Y".into())),
        );

        let next = rewrite_journal(
            &paths,
            &journal,
            OpKind::Replace {
                phase: ReplacePhase::PostReplaceMaintenance,
                old_name: "disk1".into(),
                new_name: "disk2".into(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath("/dev/disk/by-id/ata-Y".into()),
                    mapper_name: "braid-disk2".into(),
                    mode: ReplaceJournalMode::ExistingLuks {
                        luks_uuid: LuksUuid("luks-2".into()),
                        enroll_key_file: None,
                    },
                },
                source: ReplaceJournalSource::Live {
                    old_devid: 1,
                    old_mapper: MapperName("braid-disk1".into()),
                },
                restore_raid1_after_commit: true,
            },
            Some(committed.clone()),
        )
        .unwrap();

        assert_eq!(next.started_at, journal.started_at);
        assert_eq!(next.pre_membership, journal.pre_membership);
        assert_eq!(next.target_membership, committed);
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, next);
    }

    #[test]
    fn rewrite_journal_preserves_context_for_remove_missing_phase() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::RemoveMissing {
                phase: RemoveMissingPhase::PoolMutation,
                devid: 2,
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();

        let next = rewrite_journal(
            &paths,
            &journal,
            OpKind::RemoveMissing {
                phase: RemoveMissingPhase::PostRemoveMissingMaintenance,
                devid: 2,
                restore_raid1_after_commit: true,
            },
            None,
        )
        .unwrap();

        assert_eq!(next.started_at, journal.started_at);
        assert_eq!(next.pre_membership, journal.pre_membership);
        assert_eq!(next.target_membership, journal.target_membership);
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, next);
    }
}
