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
            .unwrap_or_else(|| {
                // Fallback: we don't know the by_id — this shouldn't happen
                // in practice since the device was in one of the snapshots
                crate::types::ByIdPath(format!("unknown-{}", dev.mapper.0))
            });
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
