use crate::membership::{LuksUuidMap, PoolMembership};
use crate::state_io::atomic_write;
use crate::state_paths::StatePaths;
use crate::types::{
    ByIdPath, Devid, DiskName, Fsid, KeyFilePath, LuksFormatExtraOpts, LuksUuid, MapperName,
};
use crate::util::now_iso;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

const PENDING_OP_MANUAL_REMEDIATION: &str = "Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run.";

/// A pending-operation journal records the full context of a mutation in progress.
/// When this file exists, braid enters recovery mode: `add`, `remove`,
/// `remove-missing`, `replace`, `unlock`, `enroll`, and `discover --write`
/// hard-fail, while read-only diagnostics and cleanup surfaces (`status`,
/// `doctor`, `lock`, bare `discover`) stay available. `recover` is the only
/// command that clears the journal. The two embedded
/// `PoolMembership` snapshots are load-bearing identity surfaces (see plan
/// "Accepted risk: journal-as-identity trust surface"); `deny_unknown_fields`
/// rejects unknown top-level keys to catch coherent hand-edits at load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub started_at: String,
    pub op: OpKind,
    /// Snapshot of pool.json at journal write time -- known-good state before the mutation.
    pub pre_membership: PoolMembership,
    /// What pool.json should become if the mutation succeeds.
    pub target_membership: PoolMembership,
}

impl Journal {
    /// True when this journal records the first add into a
    /// previously empty pool. `pre_membership` is empty, so recovery
    /// has no prior pool state to fall back to and must mount the
    /// targets being added instead.
    pub fn is_bootstrap_add(&self) -> bool {
        matches!(self.op, OpKind::Add { .. }) && self.pre_membership.is_empty()
    }
}

/// Phase marker for `OpKind::Add`. Distinguishes pre-commit (PoolMutation)
/// from post-commit (PostAddBalanceRaid1) so recovery replay routes to the
/// correct step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AddPhase {
    PoolMutation,
    PostAddBalanceRaid1,
}

/// One per-disk target within an `OpKind::Add` invocation. The map key in
/// `OpKind::Add.targets` is the authoritative `LuksUuid` identity; this
/// struct carries presentation (`name`) and hardware addressing (`by_id`)
/// plus the per-target mode. `name` is required so replay derives the
/// mapper via `config::mapper_name` and the label via
/// `config::luks_label_for` at the call site that builds
/// `CryptsetupLuksFormat`; the struct itself never stores `luks_uuid`,
/// `mapper_name`, `label`, or `extra_opts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AddJournalTarget {
    pub name: DiskName,
    pub by_id: ByIdPath,
    pub mode: AddJournalMode,
}

/// Per-target mode for an `OpKind::Add` entry. Identity (`LuksUuid`) lives
/// only as the surrounding `LuksUuidMap` key; mode-specific fields stay
/// inside the variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum AddJournalMode {
    /// Adoption of a returning braid-labeled disk whose pool FSID has been
    /// verified at planning time. `verified_pool_fsid` backstops the
    /// Add-recovery FSID cross-check that the UUID gate does not subsume
    /// (see plan lines 979-987).
    RecoverableBraidLabeled {
        verified_pool_fsid: Fsid,
        /// Keyfile to enroll into LUKS slot 1 if `add --enroll DIR` was
        /// passed against this returning braid disk and slot 1 was empty
        /// at planning time. `None` covers both the no-`--enroll` case
        /// and the idempotent-skip case (slot 1 already authenticates).
        /// Recovery replays `cryptsetup luksAddKey` + `luksHeaderBackup`
        /// before pool_add_device when this is `Some`.
        enroll_key_file: Option<KeyFilePath>,
    },
    /// Fresh `cryptsetup luksFormat` of a non-LUKS or wipeable disk. The
    /// label is derived via `config::luks_label_for` at the format call
    /// site; only the structured `extra_opts` argv slice is journaled.
    FreshLuks {
        extra_opts: LuksFormatExtraOpts,
        enroll_key_file: Option<KeyFilePath>,
    },
}

/// Phase marker for `OpKind::RemoveMissing`. PoolMutation gates the
/// destructive `btrfs device remove missing`; PostRemoveMissingMaintenance
/// gates the (optional) raid1 restore balance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoveMissingPhase {
    PoolMutation,
    PostRemoveMissingMaintenance,
}

/// Phase marker for `OpKind::Replace`. PoolMutation gates the
/// `btrfs replace start` request; PostReplaceMaintenance gates the
/// (optional) post-commit restore balance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplacePhase {
    PoolMutation,
    PostReplaceMaintenance,
}

/// Source-side description for `OpKind::Replace`. `Live` records the
/// observed source mapper for the post-commit close; `Missing` records
/// only the devid because no live mapper exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplaceJournalSource {
    Live {
        old_devid: Devid,
        /// Pattern #1: the observed mapper for the post-commit
        /// `close_mapper_best_effort` call and the recovery mirror in
        /// `execute_replace_post_maintenance_recovery` /
        /// `close_old_mapper_best_effort`. Journaled at plan time so a
        /// drifted mapper between plan and post-commit close still targets
        /// the right dm slot. Identity decisions read `old_uuid` at the op
        /// level; this field is consulted only for the close. Parallels
        /// `lock.rs`'s "close observed, not reconstructed" doctrine for the
        /// same drift-safety reason.
        old_mapper: MapperName,
    },
    Missing {
        old_devid: Devid,
    },
}

/// New-target descriptor for `OpKind::Replace`. The mapper is derived
/// from `OpKind::Replace.new_name` at the format/open call site, not
/// stored here; this struct never carries `mapper_name`, value-side
/// UUID, label, or `extra_opts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplaceJournalTarget {
    pub by_id: ByIdPath,
    pub mode: ReplaceJournalMode,
}

/// Mode-specific data for the new disk in `OpKind::Replace`. Identity
/// lives at the op level as `new_uuid`; this enum carries only the
/// per-mode argv extras or adoption metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum ReplaceJournalMode {
    /// Fresh `cryptsetup luksFormat` of the new disk. Label is derived
    /// via `config::luks_label_for` at the format call site; only
    /// `extra_opts` is journaled.
    FreshLuks {
        extra_opts: LuksFormatExtraOpts,
        enroll_key_file: Option<KeyFilePath>,
    },
    /// Adoption of an already-LUKS new disk. Identity is `new_uuid` at
    /// the op level; only the enroll keyfile (if any) lives here.
    ExistingLuks {
        /// Keyfile to enroll into LUKS slot 1 if `replace --enroll DIR`
        /// was passed against an already-formatted new disk and slot 1
        /// was empty at planning time. `None` covers both no-`--enroll`
        /// and the idempotent-skip case. Recovery replays
        /// `cryptsetup luksAddKey` + `luksHeaderBackup` after the LUKS
        /// UUID identity probe and credential verification when `Some`.
        enroll_key_file: Option<KeyFilePath>,
    },
}

/// Discriminated union of every mutating operation braid journals. The
/// container-level `#[serde(deny_unknown_fields)]` catches unknown
/// top-level keys alongside the `op` discriminator for every variant,
/// pinning the contract that hand-edits cannot resurrect dropped fields
/// such as `mapper_name`, `luks_label`, or value-side `luks_uuid`.
// One in-flight OpKind per CLI invocation; boxing the ~200-byte ReplaceJournalTarget variant would litter every match arm and serde derive without measurable benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", deny_unknown_fields)]
pub enum OpKind {
    Add {
        phase: AddPhase,
        targets: LuksUuidMap<AddJournalTarget>,
    },
    /// Remove a live member. Identity is `luks_uuid`; `name` is rendered
    /// in progress and recovery diagnostic messages only and never drives
    /// any planning, gating, or identity decision (see plan
    /// "`OpKind::Remove.name` log surfaces").
    Remove { luks_uuid: LuksUuid, name: DiskName },
    RemoveMissing {
        phase: RemoveMissingPhase,
        devid: Devid,
        restore_raid1_after_commit: bool,
    },
    /// Replace one member with another. `old_uuid` and `new_uuid` are the
    /// authoritative identities consulted by every planning, recovery,
    /// and close-time check; the `*_name` fields exist for log and
    /// recovery-diagnostic rendering only.
    Replace {
        phase: ReplacePhase,
        old_uuid: LuksUuid,
        old_name: DiskName,
        new_uuid: LuksUuid,
        new_name: DiskName,
        new_target: ReplaceJournalTarget,
        source: ReplaceJournalSource,
        restore_raid1_after_commit: bool,
    },
}

/// Pinned error inventory for `pending-op.json` I/O. The plan locks the
/// variant set to exactly Parse/Io/Save; Parse is the only variant whose
/// `Display` text is pinned verbatim (so `docs/internals/luks-unlock.md` can quote
/// it). `Io` covers read failures and `Save` covers write-side IO
/// (including durable delete) and serialization failures.
#[derive(Debug, Error)]
pub enum JournalError {
    #[error(
        "failed to parse pending-op.json: {detail}. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run."
    )]
    Parse { detail: String },

    #[error("failed to read pending-op.json at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write pending-op.json at {path}: {source}")]
    Save {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Write the pending operation journal atomically. Serialization errors
/// are surfaced through `JournalError::Save` so the pinned inventory holds
/// without a separate Serialize variant -- the failure mode is "could not
/// produce a writable representation of the journal at this path".
pub fn write_journal(paths: &StatePaths, journal: &Journal) -> Result<(), JournalError> {
    let path = paths.pending_op_json();
    let json = serde_json::to_string_pretty(journal).map_err(|e| JournalError::Save {
        path: path.clone(),
        source: std::io::Error::other(e.to_string()),
    })?;
    atomic_write(&path, json.as_bytes()).map_err(|source| JournalError::Save { path, source })
}

/// Load the pending-operation journal if present. Returns `None` when the
/// file does not exist; `Io` for any other read failure; `Parse` for any
/// serde failure (including LuksUuidMap canonicalization rejections and
/// `deny_unknown_fields` rejections of hand-edited extras).
pub fn load_journal(paths: &StatePaths) -> Result<Option<Journal>, JournalError> {
    let path = paths.pending_op_json();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(JournalError::Io { path, source }),
    };
    let journal: Journal = serde_json::from_str(&contents).map_err(|e| JournalError::Parse {
        detail: e.to_string(),
    })?;
    Ok(Some(journal))
}

/// Status-facing recovery-mode advisory so read-only triage surfaces an owed
/// `recover` without making status depend on recovery execution.
pub fn pending_op_advisories(paths: &StatePaths) -> Vec<String> {
    match load_journal(paths) {
        Ok(Some(journal)) => vec![format!(
            "interrupted operation detected (pending-op.json exists, started {}) -- run 'braid recover' to reconcile",
            journal.started_at
        )],
        Ok(None) => vec![],
        Err(e @ JournalError::Parse { .. }) => vec![e.to_string()],
        Err(e) => vec![format!("{e}. {PENDING_OP_MANUAL_REMEDIATION}")],
    }
}

/// Durably delete the journal file. Fsyncs the parent directory after
/// removal so the deletion survives power loss. Returns `Ok` if the file
/// does not exist. Failures surface as `JournalError::Save` because the
/// pinned variant inventory does not carry a dedicated Delete role and
/// the operation is a state-write on `pending-op.json` (see plan lines
/// 1120-1140).
pub fn clear_journal(paths: &StatePaths) -> Result<(), JournalError> {
    let path = paths.pending_op_json();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let dir = path.parent().ok_or_else(|| JournalError::Save {
                path: path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "journal path has no parent directory",
                ),
            })?;
            crate::state_io::sync_dir(dir).map_err(|source| JournalError::Save {
                path: path.clone(),
                source,
            })?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(JournalError::Save { path, source }),
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
    use crate::membership::{LuksUuidMap, PoolMembership};
    use crate::state_paths::StatePaths;
    use crate::test_fixtures::{disk_member, test_uuid};

    // Test-module seed allocation: cli/src/journal.rs uses 201-299. See
    // cli/src/cmd.rs tests module for the cross-module map (membership.rs
    // 100-199, luks.rs 200, journal.rs 201-299, cmd.rs 300-399).

    // -------------------------------------------------------------------
    // Builders
    // -------------------------------------------------------------------

    fn membership_with(entries: Vec<(LuksUuid, crate::membership::DiskMember)>) -> PoolMembership {
        let mut m = PoolMembership::empty();
        for (uuid, member) in entries {
            m.insert(uuid, member).expect("test fixture insert");
        }
        m
    }

    fn sample_membership() -> PoolMembership {
        membership_with(vec![disk_member(201, "disk1", "/dev/disk/by-id/ata-X")])
    }

    fn add_target(name: &str, by_id: &str) -> AddJournalTarget {
        AddJournalTarget {
            name: DiskName::parse(name).unwrap(),
            by_id: ByIdPath::parse(by_id).unwrap(),
            mode: AddJournalMode::FreshLuks {
                extra_opts: LuksFormatExtraOpts::parse(&[]).unwrap(),
                enroll_key_file: None,
            },
        }
    }

    fn add_targets_map(
        entries: Vec<(LuksUuid, AddJournalTarget)>,
    ) -> LuksUuidMap<AddJournalTarget> {
        let mut map = LuksUuidMap::new();
        for (uuid, t) in entries {
            map.insert(uuid, t).expect("test add-target insert");
        }
        map
    }

    // -------------------------------------------------------------------
    // Round-trip: every new OpKind shape
    // -------------------------------------------------------------------

    /// Intent: an `OpKind::Add` carrying multiple targets round-trips
    /// through `write_journal` + `load_journal` against a real tempfile
    /// path with deterministic UUID-sorted key order independent of
    /// insertion order.
    ///
    /// Why it exists: regression on either the `LuksUuidMap` canonicalizing
    /// Deserialize or the `BTreeMap` key ordering would silently flip
    /// recovery replay's notion of "which target was first" and break the
    /// pinned `first collision wins` ordering for `AddError::DuplicateUuid`.
    ///
    /// Scenario: operator runs `braid add disk2=... disk3=... disk1=...`
    /// and the journal is read back after a crash -- the targets must
    /// surface in UUID order regardless of CLI input order.
    #[test]
    fn roundtrip_add_multi_target_uuid_sorted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        let u_a = test_uuid(210);
        let u_b = test_uuid(211);
        let u_c = test_uuid(212);

        // Insert in non-sorted order so the iteration order is exercised.
        let targets = add_targets_map(vec![
            (u_b.clone(), add_target("disk2", "/dev/disk/by-id/ata-B")),
            (u_a.clone(), add_target("disk1", "/dev/disk/by-id/ata-A")),
            (u_c.clone(), add_target("disk3", "/dev/disk/by-id/ata-C")),
        ]);

        let pre = membership_with(vec![
            disk_member(213, "p2", "/dev/disk/by-id/ata-PRE-B"),
            disk_member(214, "p1", "/dev/disk/by-id/ata-PRE-A"),
        ]);
        let post = membership_with(vec![
            disk_member(215, "t3", "/dev/disk/by-id/ata-TGT-C"),
            disk_member(216, "t1", "/dev/disk/by-id/ata-TGT-A"),
        ]);

        let journal = build_journal(
            pre,
            post,
            OpKind::Add {
                phase: AddPhase::PoolMutation,
                targets,
            },
        );

        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().expect("Some");
        assert_eq!(loaded, journal);

        // The on-disk JSON has UUID-sorted keys inside `targets`.
        let body = std::fs::read_to_string(paths.pending_op_json()).unwrap();
        let pos_a = body.find(u_a.as_str()).expect("u_a present");
        let pos_b = body.find(u_b.as_str()).expect("u_b present");
        let pos_c = body.find(u_c.as_str()).expect("u_c present");
        assert!(
            pos_a < pos_b && pos_b < pos_c,
            "targets must be UUID-sorted in on-disk JSON"
        );

        // And pre/target memberships use sorted UUID iteration too.
        let observed: Vec<&LuksUuid> = loaded.target_membership.iter().map(|(u, _)| u).collect();
        let mut want = observed.clone();
        want.sort();
        assert_eq!(observed, want);
    }

    #[test]
    fn roundtrip_remove_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(220);
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::Remove {
                luks_uuid: u.clone(),
                name: DiskName::parse("disk1").unwrap(),
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, journal);
        // luks_uuid is the authoritative identity; name is rendered for logs only.
        if let OpKind::Remove { luks_uuid, name } = loaded.op {
            assert_eq!(luks_uuid, u);
            assert_eq!(name.as_str(), "disk1");
        } else {
            panic!("expected OpKind::Remove");
        }
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
                devid: Devid::new(3),
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, journal);
    }

    #[test]
    fn roundtrip_replace_existing_luks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(230);
        let u_new = test_uuid(231);
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Replace {
                phase: ReplacePhase::PoolMutation,
                old_uuid: u_old.clone(),
                old_name: DiskName::parse("disk1").unwrap(),
                new_uuid: u_new.clone(),
                new_name: DiskName::parse("disk2").unwrap(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/ata-NEW").unwrap(),
                    mode: ReplaceJournalMode::ExistingLuks {
                        enroll_key_file: Some(KeyFilePath::new(PathBuf::from(
                            "/run/keys/braid.key",
                        ))),
                    },
                },
                source: ReplaceJournalSource::Live {
                    old_devid: Devid::new(1),
                    old_mapper: MapperName::from_basename("braid-disk1".into()),
                },
                restore_raid1_after_commit: false,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let body = std::fs::read_to_string(paths.pending_op_json()).unwrap();
        let val: serde_json::Value = serde_json::from_str(&body).unwrap();
        let op = val.get("op").unwrap();
        assert!(
            op.get("new_by_id").is_none(),
            "replace journal must not duplicate new target by-id at op root"
        );
        assert_eq!(
            op.pointer("/new_target/by_id").and_then(|v| v.as_str()),
            Some("/dev/disk/by-id/ata-NEW")
        );
        // No mapper_name on the journaled target.
        assert!(op.pointer("/new_target/mapper_name").is_none());
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, journal);
    }

    #[test]
    fn roundtrip_replace_fresh_luks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(232);
        let u_new = test_uuid(233);
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Replace {
                phase: ReplacePhase::PoolMutation,
                old_uuid: u_old.clone(),
                old_name: DiskName::parse("disk1").unwrap(),
                new_uuid: u_new.clone(),
                new_name: DiskName::parse("disk2").unwrap(),
                new_target: ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/ata-NEW").unwrap(),
                    mode: ReplaceJournalMode::FreshLuks {
                        extra_opts: LuksFormatExtraOpts::parse(&[
                            "--perf-no_read_workqueue".to_owned()
                        ])
                        .unwrap(),
                        enroll_key_file: None,
                    },
                },
                source: ReplaceJournalSource::Missing {
                    old_devid: Devid::new(7),
                },
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, journal);
        // No luks_label on the journaled mode.
        let body = std::fs::read_to_string(paths.pending_op_json()).unwrap();
        assert!(!body.contains("luks_label"));
    }

    /// Roundtrip the `Some(kf)` shape of
    /// `AddJournalMode::RecoverableBraidLabeled`. Catching a serde drift
    /// on the populated arm specifically (not just `None`) keeps the
    /// recovery contract observable.
    #[test]
    fn roundtrip_add_recoverable_with_enroll_key_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(240);
        let targets = add_targets_map(vec![(
            u,
            AddJournalTarget {
                name: DiskName::parse("disk2").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/ata-Y").unwrap(),
                mode: AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                        .unwrap(),
                    enroll_key_file: Some(KeyFilePath::new(PathBuf::from("/run/keys/braid.key"))),
                },
            },
        )]);
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Add {
                phase: AddPhase::PoolMutation,
                targets,
            },
        );
        write_journal(&paths, &journal).unwrap();
        let loaded = load_journal(&paths).unwrap().unwrap();
        assert_eq!(loaded, journal);
    }

    // -------------------------------------------------------------------
    // load_missing_returns_none, clear, durability
    // -------------------------------------------------------------------

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        assert!(load_journal(&paths).unwrap().is_none());
    }

    // Intent: pending-op status advisories stay quiet when no recovery journal exists.
    // Why it exists: `braid status` must not show recovery guidance for the normal idle state.
    // Scenario: operator checks an offline or healthy pool with no interrupted mutation in progress.
    #[test]
    fn pending_op_advisories_empty_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        assert!(pending_op_advisories(&paths).is_empty());
    }

    // Intent: a valid pending-op journal produces one status advisory with the journal timestamp.
    // Why it exists: `braid status` is the recovery-mode triage command and must point operators to `braid recover`.
    // Scenario: a mutation wrote `pending-op.json` and then stopped before reconciliation completed.
    #[test]
    fn pending_op_advisories_present_includes_started_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let started_at = "2026-05-20T10:30:00Z";
        let journal = Journal {
            started_at: started_at.to_owned(),
            op: OpKind::Remove {
                luks_uuid: test_uuid(291),
                name: DiskName::parse("disk1").unwrap(),
            },
            pre_membership: sample_membership(),
            target_membership: PoolMembership::empty(),
        };
        write_journal(&paths, &journal).unwrap();

        let advisories = pending_op_advisories(&paths);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains("interrupted operation detected"));
        assert!(advisories[0].contains(started_at));
        assert!(advisories[0].contains("braid recover"));
    }

    // Intent: an unparseable pending-op journal produces the canonical manual remediation phrase.
    // Why it exists: `braid recover` also loads the journal, so pointing there would create a recover/status loop.
    // Scenario: an operator or disk fault leaves `pending-op.json` present but not valid JSON.
    #[test]
    fn pending_op_advisories_unparseable_uses_canonical_remediation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::write(paths.pending_op_json(), "not json").unwrap();

        let advisories = pending_op_advisories(&paths);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains(
            "Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run."
        ));
        assert!(!advisories[0].contains("braid recover"));
    }

    // Intent: pending-op read failures also produce the canonical manual remediation phrase.
    // Why it exists: non-parse load failures cannot be reconciled by `braid recover` either.
    // Scenario: `pending-op.json` is present as an unreadable filesystem object instead of a readable file.
    #[test]
    fn pending_op_advisories_io_error_uses_canonical_remediation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::create_dir(paths.pending_op_json()).unwrap();

        let advisories = pending_op_advisories(&paths);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains(
            "Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run."
        ));
        assert!(!advisories[0].contains("braid recover"));
    }

    #[test]
    fn clear_removes_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(250);
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::Remove {
                luks_uuid: u,
                name: DiskName::parse("disk1").unwrap(),
            },
        );
        write_journal(&paths, &journal).unwrap();
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
        let u = test_uuid(251);
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::Remove {
                luks_uuid: u,
                name: DiskName::parse("disk1").unwrap(),
            },
        );
        write_journal(&paths, &journal).unwrap();
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

    // -------------------------------------------------------------------
    // rewrite_journal
    // -------------------------------------------------------------------

    #[test]
    fn rewrite_journal_preserves_context_and_replaces_op() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(260);
        let targets = add_targets_map(vec![(
            u.clone(),
            AddJournalTarget {
                name: DiskName::parse("disk2").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/ata-Y").unwrap(),
                mode: AddJournalMode::FreshLuks {
                    extra_opts: LuksFormatExtraOpts::parse(&[]).unwrap(),
                    enroll_key_file: None,
                },
            },
        )]);
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            OpKind::Add {
                phase: AddPhase::PoolMutation,
                targets: targets.clone(),
            },
        );
        write_journal(&paths, &journal).unwrap();

        let committed = membership_with(vec![
            disk_member(261, "disk1", "/dev/disk/by-id/ata-X"),
            disk_member(262, "disk2", "/dev/disk/by-id/ata-Y"),
        ]);
        let next = rewrite_journal(
            &paths,
            &journal,
            OpKind::Add {
                phase: AddPhase::PostAddBalanceRaid1,
                targets,
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
        let u_old = test_uuid(263);
        let u_new = test_uuid(264);
        let mk_op = |phase| OpKind::Replace {
            phase,
            old_uuid: u_old.clone(),
            old_name: DiskName::parse("disk1").unwrap(),
            new_uuid: u_new.clone(),
            new_name: DiskName::parse("disk2").unwrap(),
            new_target: ReplaceJournalTarget {
                by_id: ByIdPath::parse("/dev/disk/by-id/ata-Y").unwrap(),
                mode: ReplaceJournalMode::ExistingLuks {
                    enroll_key_file: None,
                },
            },
            source: ReplaceJournalSource::Live {
                old_devid: Devid::new(1),
                old_mapper: MapperName::from_basename("braid-disk1".into()),
            },
            restore_raid1_after_commit: true,
        };
        let journal = build_journal(
            sample_membership(),
            sample_membership(),
            mk_op(ReplacePhase::PoolMutation),
        );
        write_journal(&paths, &journal).unwrap();

        let committed = membership_with(vec![disk_member(265, "disk2", "/dev/disk/by-id/ata-Y")]);
        let next = rewrite_journal(
            &paths,
            &journal,
            mk_op(ReplacePhase::PostReplaceMaintenance),
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
                devid: Devid::new(2),
                restore_raid1_after_commit: true,
            },
        );
        write_journal(&paths, &journal).unwrap();

        let next = rewrite_journal(
            &paths,
            &journal,
            OpKind::RemoveMissing {
                phase: RemoveMissingPhase::PostRemoveMissingMaintenance,
                devid: Devid::new(2),
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

    // -------------------------------------------------------------------
    // deny_unknown_fields + Parse error remediation
    // -------------------------------------------------------------------

    /// Non-UUID-keyed journal targets return `JournalError::Parse`
    /// whose Display contains the pinned remediation phrase verbatim.
    #[test]
    fn non_uuid_keyed_targets_fails_parse_with_remediation_phrase() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        // A targets map keyed by disk name, not UUID. LuksUuidMap's
        // canonicalizing Deserialize rejects the non-UUID key.
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Add",
                "phase": "PoolMutation",
                "targets": {{
                  "disk2": {{
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/ata-Y",
                    "mode": {{ "FreshLuks": {{ "extra_opts": [], "enroll_key_file": null }} }}
                  }}
                }}
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{ "{u}": {{ "name": "disk1", "by_id": "/dev/disk/by-id/ata-X" }} }} }}
            }}"#,
            u = test_uuid(270)
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(
            s.starts_with("failed to parse pending-op.json:"),
            "got: {s}"
        );
        assert!(
            s.contains(
                "Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run."
            ),
            "got: {s}"
        );
    }

    /// JSON containing a resurrected `luks_uuid` value-side field inside
    /// `AddJournalMode::RecoverableBraidLabeled` fails under
    /// `deny_unknown_fields` and the error names the unknown field.
    #[test]
    fn add_recoverable_resurrected_luks_uuid_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_target = test_uuid(271);
        let u_resurrected = test_uuid(272);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Add",
                "phase": "PoolMutation",
                "targets": {{
                  "{u_target}": {{
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/ata-Y",
                    "mode": {{ "RecoverableBraidLabeled": {{
                      "verified_pool_fsid": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                      "luks_uuid": "{u_resurrected}",
                      "enroll_key_file": null
                    }} }}
                  }}
                }}
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_target = u_target,
            u_resurrected = u_resurrected,
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(
            s.contains("luks_uuid"),
            "expected error to name the unknown field, got: {s}"
        );
    }

    /// JSON containing a resurrected `mapper_name` value-side field on
    /// `AddJournalTarget` fails under `deny_unknown_fields`.
    #[test]
    fn add_target_resurrected_mapper_name_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(273);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Add",
                "phase": "PoolMutation",
                "targets": {{
                  "{u}": {{
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/ata-Y",
                    "mapper_name": "braid-disk2",
                    "mode": {{ "FreshLuks": {{ "extra_opts": [], "enroll_key_file": null }} }}
                  }}
                }}
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u = u
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("mapper_name"), "got: {s}");
    }

    /// JSON containing a resurrected `luks_label` value-side field inside
    /// `AddJournalMode::FreshLuks` fails under `deny_unknown_fields`.
    #[test]
    fn add_fresh_resurrected_luks_label_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(274);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Add",
                "phase": "PoolMutation",
                "targets": {{
                  "{u}": {{
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/ata-Y",
                    "mode": {{ "FreshLuks": {{
                      "luks_label": "braid-disk2",
                      "extra_opts": [],
                      "enroll_key_file": null
                    }} }}
                  }}
                }}
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u = u
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("luks_label"), "got: {s}");
    }

    /// JSON with resurrected `mapper_name` on `ReplaceJournalTarget` fails
    /// under `deny_unknown_fields`.
    #[test]
    fn replace_target_resurrected_mapper_name_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(275);
        let u_new = test_uuid(276);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Replace",
                "phase": "PoolMutation",
                "old_uuid": "{u_old}",
                "old_name": "disk1",
                "new_uuid": "{u_new}",
                "new_name": "disk2",
                "new_target": {{
                  "by_id": "/dev/disk/by-id/ata-NEW",
                  "mapper_name": "braid-disk2",
                  "mode": {{ "ExistingLuks": {{ "enroll_key_file": null }} }}
                }},
                "source": {{ "Missing": {{ "old_devid": 7 }} }},
                "restore_raid1_after_commit": false
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_old = u_old,
            u_new = u_new,
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("mapper_name"), "got: {s}");
    }

    /// JSON with resurrected `luks_uuid` inside
    /// `ReplaceJournalMode::ExistingLuks` fails under
    /// `deny_unknown_fields`. The mode-level resurrection is the analogue
    /// of the Add `RecoverableBraidLabeled` case.
    #[test]
    fn replace_existing_resurrected_luks_uuid_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(277);
        let u_new = test_uuid(278);
        let u_resurrected = test_uuid(279);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Replace",
                "phase": "PoolMutation",
                "old_uuid": "{u_old}",
                "old_name": "disk1",
                "new_uuid": "{u_new}",
                "new_name": "disk2",
                "new_target": {{
                  "by_id": "/dev/disk/by-id/ata-NEW",
                  "mode": {{ "ExistingLuks": {{
                    "luks_uuid": "{u_resurrected}",
                    "enroll_key_file": null
                  }} }}
                }},
                "source": {{ "Missing": {{ "old_devid": 7 }} }},
                "restore_raid1_after_commit": false
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_old = u_old,
            u_new = u_new,
            u_resurrected = u_resurrected,
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("luks_uuid"), "got: {s}");
    }

    /// JSON with resurrected `luks_label` inside
    /// `ReplaceJournalMode::FreshLuks` fails under
    /// `deny_unknown_fields`.
    #[test]
    fn replace_fresh_resurrected_luks_label_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(280);
        let u_new = test_uuid(281);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Replace",
                "phase": "PoolMutation",
                "old_uuid": "{u_old}",
                "old_name": "disk1",
                "new_uuid": "{u_new}",
                "new_name": "disk2",
                "new_target": {{
                  "by_id": "/dev/disk/by-id/ata-NEW",
                  "mode": {{ "FreshLuks": {{
                    "luks_label": "braid-disk2",
                    "extra_opts": [],
                    "enroll_key_file": null
                  }} }}
                }},
                "source": {{ "Missing": {{ "old_devid": 7 }} }},
                "restore_raid1_after_commit": false
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_old = u_old,
            u_new = u_new,
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("luks_label"), "got: {s}");
    }

    /// Variant-level deny_unknown_fields symmetry pin for `OpKind::Remove`:
    /// an unknown extra top-level field alongside the legitimate keys
    /// fails. Mirrors the Add/Replace resurrected-field cases on the
    /// Remove variant so a regression that drops the container-level
    /// `deny_unknown_fields` on `OpKind` is caught here.
    #[test]
    fn op_kind_remove_rejects_unknown_top_level_field() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(282);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Remove",
                "luks_uuid": "{u}",
                "name": "disk1",
                "extra": "x"
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u = u
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("extra"), "got: {s}");
    }

    /// Variant-level deny_unknown_fields symmetry pin for
    /// `OpKind::RemoveMissing`: an unknown extra field alongside the
    /// legitimate keys fails.
    #[test]
    fn op_kind_remove_missing_rejects_unknown_top_level_field() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let body = r#"{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {
                "op": "RemoveMissing",
                "phase": "PoolMutation",
                "devid": 7,
                "restore_raid1_after_commit": true,
                "extra": "x"
              },
              "pre_membership": { "disks": {} },
              "target_membership": { "disks": {} }
            }"#;
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("extra"), "got: {s}");
    }

    /// An unknown top-level key in `pending-op.json` alongside the valid
    /// Journal fields fails through `Journal`'s `deny_unknown_fields`.
    /// This pins plan line 446's fourth case.
    #[test]
    fn unknown_top_level_key_in_pending_op_json_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u = test_uuid(283);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Remove",
                "luks_uuid": "{u}",
                "name": "disk1"
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }},
              "schema_version": 1
            }}"#,
            u = u
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("schema_version"), "got: {s}");
    }

    // -------------------------------------------------------------------
    // Invalid UUID rejections at load_journal
    // -------------------------------------------------------------------

    /// Invalid UUID in the Add map key fails at `load_journal` (the error
    /// originates from `LuksUuidMap`'s canonicalizing Deserialize and is
    /// surfaced through the journal parse layer).
    #[test]
    fn add_invalid_uuid_in_targets_map_key_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let body = r#"{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {
                "op": "Add",
                "phase": "PoolMutation",
                "targets": {
                  "not-a-uuid": {
                    "name": "disk2",
                    "by_id": "/dev/disk/by-id/ata-Y",
                    "mode": { "FreshLuks": { "extra_opts": [], "enroll_key_file": null } }
                  }
                }
              },
              "pre_membership": { "disks": {} },
              "target_membership": { "disks": {} }
            }"#;
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
        let s = err.to_string();
        assert!(s.contains("invalid LUKS UUID"), "got: {s}");
    }

    /// Invalid UUID in `OpKind::Remove.luks_uuid` fails at `load_journal`.
    #[test]
    fn remove_invalid_luks_uuid_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let body = r#"{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {
                "op": "Remove",
                "luks_uuid": "not-a-uuid",
                "name": "disk1"
              },
              "pre_membership": { "disks": {} },
              "target_membership": { "disks": {} }
            }"#;
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
    }

    /// Invalid UUID in `OpKind::Replace.old_uuid` fails at `load_journal`.
    #[test]
    fn replace_invalid_old_uuid_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_new = test_uuid(284);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Replace",
                "phase": "PoolMutation",
                "old_uuid": "not-a-uuid",
                "old_name": "disk1",
                "new_uuid": "{u_new}",
                "new_name": "disk2",
                "new_target": {{
                  "by_id": "/dev/disk/by-id/ata-NEW",
                  "mode": {{ "ExistingLuks": {{ "enroll_key_file": null }} }}
                }},
                "source": {{ "Missing": {{ "old_devid": 7 }} }},
                "restore_raid1_after_commit": false
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_new = u_new
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
    }

    /// Invalid UUID in `OpKind::Replace.new_uuid` fails at `load_journal`.
    #[test]
    fn replace_invalid_new_uuid_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let u_old = test_uuid(285);
        let body = format!(
            r#"{{
              "started_at": "2026-05-12T00:00:00Z",
              "op": {{
                "op": "Replace",
                "phase": "PoolMutation",
                "old_uuid": "{u_old}",
                "old_name": "disk1",
                "new_uuid": "not-a-uuid",
                "new_name": "disk2",
                "new_target": {{
                  "by_id": "/dev/disk/by-id/ata-NEW",
                  "mode": {{ "ExistingLuks": {{ "enroll_key_file": null }} }}
                }},
                "source": {{ "Missing": {{ "old_devid": 7 }} }},
                "restore_raid1_after_commit": false
              }},
              "pre_membership": {{ "disks": {{}} }},
              "target_membership": {{ "disks": {{}} }}
            }}"#,
            u_old = u_old
        );
        std::fs::write(paths.pending_op_json(), body).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
    }

    // -------------------------------------------------------------------
    // Phase enum round-trips
    // -------------------------------------------------------------------

    /// Every variant of `AddPhase`, `ReplacePhase`, and
    /// `RemoveMissingPhase` survives serialize + deserialize, proving the
    /// migration did not regress replay state shape.
    #[test]
    fn phase_enums_round_trip_every_variant() {
        for p in [AddPhase::PoolMutation, AddPhase::PostAddBalanceRaid1] {
            let s = serde_json::to_string(&p).unwrap();
            let back: AddPhase = serde_json::from_str(&s).unwrap();
            assert_eq!(back, p);
        }
        for p in [
            ReplacePhase::PoolMutation,
            ReplacePhase::PostReplaceMaintenance,
        ] {
            let s = serde_json::to_string(&p).unwrap();
            let back: ReplacePhase = serde_json::from_str(&s).unwrap();
            assert_eq!(back, p);
        }
        for p in [
            RemoveMissingPhase::PoolMutation,
            RemoveMissingPhase::PostRemoveMissingMaintenance,
        ] {
            let s = serde_json::to_string(&p).unwrap();
            let back: RemoveMissingPhase = serde_json::from_str(&s).unwrap();
            assert_eq!(back, p);
        }
        for s_src in [
            ReplaceJournalSource::Live {
                old_devid: Devid::new(1),
                old_mapper: MapperName::from_basename("braid-x".into()),
            },
            ReplaceJournalSource::Missing {
                old_devid: Devid::new(9),
            },
        ] {
            let s = serde_json::to_string(&s_src).unwrap();
            let back: ReplaceJournalSource = serde_json::from_str(&s).unwrap();
            assert_eq!(back, s_src);
        }
    }

    // -------------------------------------------------------------------
    // One example per JournalError variant
    // -------------------------------------------------------------------

    /// `JournalError::Parse`: corrupt JSON in `pending-op.json`.
    #[test]
    fn journal_error_parse_example() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::write(paths.pending_op_json(), "not json").unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Parse { .. }));
    }

    /// `JournalError::Io`: the path exists but is not readable as text
    /// (we create a directory in place of the file so `read_to_string`
    /// returns an IO error distinct from `NotFound`).
    #[test]
    fn journal_error_io_example() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        // Create a directory where the file is expected so read_to_string
        // returns an IO error other than NotFound.
        std::fs::create_dir_all(paths.pending_op_json()).unwrap();
        let err = load_journal(&paths).unwrap_err();
        assert!(matches!(err, JournalError::Io { .. }));
    }

    /// `JournalError::Save`: `atomic_write` failure -- we point the state
    /// dir at a path whose parent does not exist and is not creatable
    /// (a file rather than a directory).
    #[test]
    fn journal_error_save_example() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Put a regular file where the StatePaths root is expected, so
        // `atomic_write` cannot create the parent directory for
        // `pending-op.json`.
        let file_in_place = tmp.path().join("not-a-dir");
        std::fs::write(&file_in_place, b"placeholder").unwrap();
        let paths = StatePaths::custom(file_in_place);
        let u = test_uuid(290);
        let journal = build_journal(
            sample_membership(),
            PoolMembership::empty(),
            OpKind::Remove {
                luks_uuid: u,
                name: DiskName::parse("disk1").unwrap(),
            },
        );
        let err = write_journal(&paths, &journal).unwrap_err();
        assert!(matches!(err, JournalError::Save { .. }));
    }
}
