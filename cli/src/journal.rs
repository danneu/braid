use crate::membership::PoolMembership;
use crate::state_io::atomic_write;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use crate::util::now_iso;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
#[serde(tag = "op")]
pub enum OpKind {
    Add {
        disks: BTreeMap<String, ByIdPath>,
    },
    Remove {
        name: String,
    },
    RemoveMissing {
        devid: Option<u64>,
    },
    Replace {
        old_name: String,
        new_name: String,
        new_by_id: ByIdPath,
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
}

/// Write the pending operation journal atomically.
pub fn write_journal(paths: &StatePaths, journal: &Journal) -> Result<(), JournalError> {
    let json = serde_json::to_string_pretty(journal).expect("Journal serialization cannot fail");
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
                disks: BTreeMap::from([("disk2".into(), ByIdPath("/dev/disk/by-id/ata-Y".into()))]),
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
                old_name: "disk1".into(),
                new_name: "disk2".into(),
                new_by_id: ByIdPath("/dev/disk/by-id/ata-NEW".into()),
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
            OpKind::RemoveMissing { devid: Some(3) },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded.op, journal.op);
    }
}
