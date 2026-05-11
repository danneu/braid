# Plan: align `evict_present_device` with the typed work plan

## Context

`cli/src/pool.rs:361-465`'s `evict_present_device` re-runs `probe_pool`
(`pool.rs:368`) and re-derives `remaining = pool.devices.len() - 1`
(`pool.rs:391`) to decide whether to issue a RAID1->single balance --
even though the planner has already committed that decision into
`RemoveWorkPlan.remaining` at `remove.rs:111` and rendered it into the
dry-run preview at `remove.rs:128-160`. Between planning and execution
the pool can change (a fourth disk auto-unlocks, a third disk goes
MISSING, btrfs flips a flapping disk's status), so the helper can:

- skip a balance the dry-run promised, leaving the user with an
  unannounced 1-disk-no-RAID1 outcome relative to what they confirmed,
  or worse,
- start an unannounced multi-hour RAID1->single balance after a
  dry-run that said "no balance needed".

The helper today does fail-close on *one* of the three drift
directions: if the target mapper is absent from the live probe
(`pool.rs:371-388`), it returns `PoolError::Failed` with
recovery-mode messaging that distinguishes hot-unplug
(null-underlying) from plain absence. That guard is sound but
*incomplete* in two ways:

1. **It only covers target-absent drift.** A non-target disk going
   MISSING (which the planner rejects at `check_no_missing_devices`,
   `remove.rs:362`) is not caught -- the helper proceeds with the
   stale `remaining` derivation. Same for a 4th disk auto-unlocking,
   which can flip a 2->1 plan into an unwanted balance against a
   3-disk pool.
2. **It runs *after* `journal::write_journal`.** The
   `evict_present_device` call site at `remove.rs:253` is below
   the journal write at `remove.rs:249`, so even the
   target-absent path strands a `pending-op.json` and forces the
   user into recovery mode. That conflicts with principle 3 at
   `docs/principles.md:23`: "Environment-side resource acquisition
   ... must happen **before** `journal::write_journal`. The journal
   write commits the user to recovery mode on any subsequent failure
   ... reorder code so any RAII guards or environment probes that
   can fail are bound above it." A pre-mutation topology probe is
   exactly such an "environment probe that can fail".

This also violates `docs/decisions/022-dry-run-preview-model.md`'s
contract: "`execute()` ... must not rediscover or reinterpret semantic
choices already made during planning." (`docs/decisions/022-dry-run-preview-model.md:45`)
ADR 022 does, however, explicitly permit "execution-time validation
that dry-run intentionally cannot do" -- so a probe is allowed at
execute time, but only as a fail-fast precondition check, never as a
second topology planner.

`evict_present_device`'s only caller is `RemovePlan::execute` at
`remove.rs:253`. The fix is a three-checkpoint design:

1. **Plan-time decision** (`plan_remove`, `RemoveWorkPlan` -- already
   exists). The planner's probe decides `needs_balance` and records
   `total`. Source of truth for topology semantics.
2. **Pre-journal clean validation.** New `validate_pool_topology` in
   `pool.rs`, called from `RemovePlan::execute` after the sleep
   inhibitor is acquired and before `journal::write_journal`.
   Validates against the planner's *identity snapshot* -- an
   `expected_present_identities: BTreeMap<MapperName, DeviceIdentity>`
   captured on `RemoveWorkPlan` at planning time, where
   `DeviceIdentity = {devid, luks_uuid}`. Not just cardinality, and
   not just mapper names. This catches: target-absent drift,
   missing-device drift, mapper-set drift (same-count swap with
   different mapper names), AND same-mapper replacement (operator
   re-opens the mapper on a different LUKS device, flipping `devid`
   or `luks_uuid` while keeping the mapper name) -- all of which
   would otherwise silently invalidate `check_eviction_space`
   capacity assumptions (`remove.rs:397`). On drift, returns `Err`
   -- the command exits cleanly without ever writing
   `pending-op.json`, matching principle 3's "command never
   started" model. Closes the long window between `plan_remove` and
   execute (e.g. user paused at the `yes` confirmation prompt).
3. **Post-journal last-moment validation.** A second
   `validate_pool_topology` call between `journal::write_journal`
   and `evict_present_device`. Same identity snapshot, same drift
   semantics. On drift, returns `Err` -- the journal is already on
   disk, so this preserves it for `braid recover`. Closes the small
   but real window between pre-journal validation and
   `pool_balance_single`. This window matters because
   `BtrfsBalanceSingle` ships `-f` (`cli/src/cmd.rs:553`), and
   btrfs-progs' `balance.c:558-561` shows that `--force` *skips* the
   10-second safety timeout that normally warns "Conversion with
   missing device(s) can be dangerous" (`balance.c:556`). With the
   timeout skipped, a disk that goes MISSING in this window would
   silently subject the pool to a dangerous profile conversion.
   Trusting "btrfs will fail safely" in this window is not safe --
   balance.c:524-569 is explicit that conversion against missing
   devices "can flip [the fs] RO due to failed metadata writes".

The helper itself becomes pure execution of the typed work plan:
slimmed signature `(runner, mapper, mount_point, needs_balance,
progress)`, no in-helper probe, no fail-closed, no `fs`. Validation
is fully owned by the call site.

Layer-2 recovery (`execute_generic_live_pool_recovery` in
`cli/src/recover.rs:951`) does its own probing
(`pool.null_underlying`, `pool.missing_devids`) and reconstructs
membership. Removing the helper's fail-closed does not relocate any
unique recovery semantics: the post-journal validation-failure
journey runs through the existing recover path.

However, the existing `OpKind::Remove` recovery guard at
`recover.rs:962-981` only restores the *target* mapper from
`pre_membership` when the live probe shows it as null-underlying or
in `missing_devids`. `build_membership_from_live_pool` (`recover.rs:1732-1770`)
walks `pool.devices` only, so a non-target disk that went MISSING
between `journal::write_journal` and the post-journal validation
failure would be *pruned* from `pool.json` by recover, even though
no btrfs mutation occurred. That makes our new post-journal gate
unsound: it preserves the journal correctly, but the recovery flow
the journal hands off to silently loses non-target state.

This plan therefore includes a small recover-side hardening: extend
the `OpKind::Remove` guard to preserve *every* `pre_membership` disk
still owned by btrfs (in `pool.null_underlying` or
`pool.missing_devids`), not just the target. With that, the
post-journal validation gate's recovery contract becomes
"`pool.json` membership is preserved against any subset of pre-op
disks the kernel still considers present-but-unreachable", which is
the contract the post-journal gate actually needs.

The historical justification for the helper (commit `7aa3b22` extracted
it as shared between `remove` and `replace`) has eroded -- `replace`'s
live path uses `pool_replace_device` (`btrfs replace start`) instead,
so today the helper has a single caller and removing the in-helper
probe carries no second-consumer risk.

## Approach

### 1. New validation helper (`cli/src/pool.rs`)

The helper validates the live pool against an *exact identity
snapshot* the planner captured -- not just cardinality, and not
just mapper names. The full identity for each present device is
`{mapper, devid, luks_uuid}`, all already produced by `probe_pool`
on `PoolDevice` (`cli/src/probe.rs:306-311`) and persisted on
`pool.json` after every mutation (`cli/src/membership.rs:181`).
Cardinality alone allows same-count survivor swaps; mapper-set
alone allows same-mapper replacement (operator runs `cryptsetup
close` + `cryptsetup open` on a different LUKS device under the
same `braid-<name>` mapper, or a flapping disk gets replaced with
a same-named different LUKS device between plan and execute).
Both leave `check_eviction_space` (`remove.rs:397`) capacity
assumptions stale. Storing the planner's identity *map* on
`RemoveWorkPlan` and comparing it equal at execute time closes
both gaps.

The helper returns a structured `TopologyDrift` so call sites can
branch on the drift *kind* (specifically the target-hot-unplug case)
and surface the rich operational guidance the existing in-helper
fail-closed (`pool.rs:373-388`) provides today, which the test at
`pool.rs:1507-1554` pins via assertions on `device: (null)`,
`hot-unplug`, `braid recover`, `braid lock`/`braid unlock`, and
`reboot`. Reducing this to a single string would lose that UX.

```rust
use std::collections::BTreeMap;
use crate::types::{LuksUuid, MapperName};

/// Identity record per present device, captured from `PoolDevice`
/// at planning time and again at validation time. Equality fails on
/// any field difference: a same-mapper replacement (e.g. cryptsetup
/// close + open on a different LUKS device under the same braid-
/// mapper between plan and execute) flips `devid` and/or `luks_uuid`,
/// which the validation must catch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub devid: u64,
    pub luks_uuid: LuksUuid,
}

/// Structured drift summary returned by `validate_pool_topology` on
/// mismatch. Position-neutral: the caller decides remediation phrasing.
pub struct TopologyDrift {
    /// None when the probe succeeded; Some when probing itself failed.
    pub probe_error: Option<String>,
    pub target_present: bool,
    pub target_null_underlying: bool,
    pub expected_present_identities: BTreeMap<MapperName, DeviceIdentity>,
    pub observed_present_identities: BTreeMap<MapperName, DeviceIdentity>,
    pub observed_missing_count: u64,
}

impl TopologyDrift {
    /// True when the target mapper is gone AND its dm node still
    /// resolves but cryptsetup reports `device: (null)`. Distinct
    /// recovery flow: re-plug does NOT self-heal the mapper.
    pub fn is_target_hot_unplug(&self) -> bool {
        !self.target_present && self.target_null_underlying
    }

    /// Brief drift summary for embedding in error messages.
    /// Mirrors the wording style of today's in-helper fail-closed
    /// at `pool.rs:373-388` so existing reader expectations carry
    /// over. Callers append remediation. Distinguishes
    /// mapper-set drift from same-mapper identity drift in the
    /// summary so an operator can tell whether a disk was added/
    /// removed vs. replaced under the same name.
    pub fn detail(&self) -> String { /* ... */ }
}

/// Execution-time validation that the live pool topology matches
/// the planner's exact identity snapshot. Returns `Err(TopologyDrift)`
/// on probe failure, target absence, missing-device drift, set
/// mismatch (any added/removed mapper), or per-mapper identity
/// drift (devid or luks_uuid changed for the same mapper name).
///
/// **Position-dependent failure semantics.** This helper is
/// position-neutral; the caller chooses where to call it and wraps
/// the returned drift with the appropriate remediation:
/// - Pre-`journal::write_journal` (clean failure path): caller wraps
///   with "Re-run `braid remove` after resolving the drift". Failure
///   here is a "command never started" exit and must NOT strand
///   `pending-op.json` (principle 3, `docs/principles.md:23`).
/// - Post-`journal::write_journal` (recovery-handoff path): caller
///   wraps with "Run `braid recover` to reconcile". Failure here
///   intentionally preserves `pending-op.json` so the recovery flow
///   can replay/reconcile.
///
/// The helper does NOT format remediation itself: doing so locks in
/// one position's UX and would mislead users on the other path
/// (e.g. telling them to re-run `braid remove` when
/// `check_no_pending_operation` at `cli/src/preflight.rs:42-54`
/// would block the re-run and direct them to `braid recover`).
pub fn validate_pool_topology<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    target_mapper: &str,
    expected_present_identities: &BTreeMap<MapperName, DeviceIdentity>,
) -> Result<(), TopologyDrift> {
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        Err(e) => {
            return Err(TopologyDrift {
                probe_error: Some(e.to_string()),
                target_present: false,
                target_null_underlying: false,
                expected_present_identities: expected_present_identities.clone(),
                observed_present_identities: BTreeMap::new(),
                observed_missing_count: 0,
            });
        }
    };
    let observed_present_identities: BTreeMap<MapperName, DeviceIdentity> = pool
        .devices
        .iter()
        .map(|d| {
            (
                d.mapper.clone(),
                DeviceIdentity {
                    devid: d.devid,
                    luks_uuid: d.luks_uuid.clone(),
                },
            )
        })
        .collect();
    let target_present = observed_present_identities
        .keys()
        .any(|m| m.0 == target_mapper);
    let target_null_underlying = pool
        .null_underlying
        .iter()
        .any(|n| n.mapper.0 == target_mapper);

    let identities_match = observed_present_identities == *expected_present_identities;
    let no_missing = pool.missing_count == 0;
    if identities_match && no_missing {
        return Ok(());
    }
    Err(TopologyDrift {
        probe_error: None,
        target_present,
        target_null_underlying,
        expected_present_identities: expected_present_identities.clone(),
        observed_present_identities,
        observed_missing_count: pool.missing_count,
    })
}
```

`BTreeMap` equality is structural over `(key, value)` pairs, so the
single comparison subsumes:
- target presence (target is in expected; if absent in observed, the
  maps differ at the target's key);
- count drift (any added/removed mapper changes the key set);
- same-mapper-different-name swap (e.g. disk3 -> disk4 with same
  count -- key set differs);
- *and* same-mapper-replacement (mapper key matches, but `devid` or
  `luks_uuid` value differs -- map equality flips on the value
  difference).

The `target_present` and `target_null_underlying` derived fields are
kept on the struct so callers can route the hot-unplug case to its
richer remediation without re-probing.

`RemoveWorkPlan` gains an `expected_present_identities:
BTreeMap<MapperName, DeviceIdentity>` field, populated at planning
time from `pool.devices` by mapping each `PoolDevice` to a
`(mapper.clone(), DeviceIdentity { devid, luks_uuid: luks_uuid.clone() })`.
The existing `total: usize` field becomes redundant
(`expected_present_identities.len()` is the same value) but can
stay for now since other planner code may use it; auditing that is
out of scope.

**Compile prerequisites**:

1. `MapperName` is defined at `cli/src/types.rs:13-15` with
   `#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize,
   Deserialize)]`. `BTreeMap` requires the key type implement `Ord`,
   so `MapperName`'s derive list must be extended with `PartialOrd,
   Ord`. Sibling newtypes in the same file (`ByIdPath`, `LuksUuid`,
   `MountPoint`) are unchanged -- adding the derives only to
   `MapperName` keeps the patch minimal.
2. `LuksUuid` is also defined in `cli/src/types.rs:9-11` with
   `#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize,
   Deserialize)]`. The new `DeviceIdentity` struct derives only
   `PartialEq, Eq` (plus `Debug, Clone`), so `LuksUuid`'s existing
   derives are sufficient -- no change needed there.
3. Import paths in `cli/src/pool.rs`: `use crate::types::{LuksUuid,
   MapperName};`. (The types live in `types`;
   `config::mapper_name(...)` is a constructor function, not the
   type's home.)

### 2. Slimmed helper signature (`cli/src/pool.rs:361`)

```rust
pub fn evict_present_device<R: CommandRunner + Sync>(
    runner: &R,
    mapper: &str,
    mount_point: &MountPoint,
    needs_balance: bool,
    progress: ProgressOutput,
) -> Result<(), PoolError>
```

Body changes:

- Replace the `if remaining == 1` branch (`pool.rs:392-410`) with
  `if needs_balance`. The balance decision now comes exclusively from
  the planner-supplied parameter.
- Delete the in-helper `let pool = probe_pool(...)` call
  (`pool.rs:368`) entirely. Drop the
  `let in_pool = pool.devices.iter().any(...)` line (`pool.rs:371`)
  and the entire fail-closed `if !in_pool { ... return
  Err(PoolError::Failed(...)) }` block at `pool.rs:373-388` --
  including the null-underlying detection and recovery-mode
  messaging. Drop the `let remaining = pool.devices.len() - 1`
  derivation (`pool.rs:391`). Topology validation is now upstream of
  the journal write; the helper becomes pure execution.
- Drop the `F: Filesystem + ?Sized` type parameter and the `fs: &F`
  argument (`pool.rs:361`). With no probe inside, there is no use
  for `fs`.
- `device_path` becomes a plain `format!("/dev/mapper/{mapper}")`
  (replacing `pool.rs:370` -- it never genuinely needed the probe).
- Rewrite the helper's doc comment (`pool.rs:343-359`):
    - drop "Probes the current pool to decide if RAID1->single
      conversion is needed" from the numbered list at `pool.rs:345`.
    - delete the entire "Fail-closed: returns `PoolError::Failed`
      ..." paragraph at `pool.rs:352-359`, including the
      `execute_generic_live_pool_recovery` cross-reference.
      That paragraph documented the in-helper fail-closed; with the
      probe gone, those semantics move to `validate_pool_topology`.
    - add: "Caller is responsible for upstream topology validation
      (see `validate_pool_topology`, which the caller invokes
      pre-journal AND post-journal). This helper assumes the typed
      work plan is consistent with the live pool and executes it.
      Drift in the residual microsecond window between the
      post-journal validation and the actual btrfs command surfaces
      as a btrfs command failure (e.g. `pool_balance_single` or
      `pool_remove_device` returning `PoolError::Failed`) and
      preserves the journal for `braid recover` to reconcile."

### 3. Caller update (`cli/src/remove.rs`)

Three changes inside `RemovePlan::execute`, in order:

(a) **Insert the pre-journal validation** between sleep-inhibitor
acquisition (`remove.rs:228-235`, ending with the closing `?;`) and
the `pre_membership`/journal block at `remove.rs:237-250`. Per
principle 3 this seam must be above the journal write; per ADR 022
line 45 it is execution-time validation, not a second topology
planner.

```rust
let _sleep_inhibitor_guard = params
    .sleep_inhibitor
    .acquire("removing disk from pool")
    .map_err(...)?;

// (Pre-journal) topology drift validation -- clean failure if the
// world changed between plan_remove and here. Above journal::write_journal
// so failure does NOT strand pending-op.json (principle 3,
// docs/principles.md:23). Remediation suffix points at re-running
// `braid remove` because no journal exists. Hot-unplug variant
// surfaces a journal-free recovery sequence (re-plug, OR close +
// reopen the stale mapper via lock/unlock or reboot, then re-run);
// `braid recover` is intentionally NOT mentioned because it would
// fail at recover.rs:1086-1094 with no pending journal.
crate::pool::validate_pool_topology(
    runner,
    fs,
    &work_plan.mount_point,
    &work_plan.target_mapper.0,
    &work_plan.expected_present_identities,
)
.map_err(|drift| {
    let detail = drift.detail();
    let suffix = if drift.is_target_hot_unplug() {
        // Journal-free remediation. We are above journal::write_journal,
        // so no pending-op.json exists; `braid recover` would fail with
        // "no pending operation journal found -- nothing to recover"
        // (cli/src/recover.rs:1086-1094). The user can resolve the
        // stale dm-crypt mapping directly without recover.
        "The remove did not start. Resolve the hot-unplug by re-plugging \
         the disk, OR by closing + reopening the stale mapper (`braid lock` \
         then `braid unlock`, or reboot then `braid unlock`), then re-run \
         `braid remove`."
    } else {
        "Resolve the drift and re-run `braid remove`."
    };
    RemoveError::Validation(format!("{detail}. {suffix}"))
})?;

// Build target membership and write journal before irreversible disk op.
let pre_membership = membership::load_membership(params.paths)
    ...
journal::write_journal(params.paths, &journal)?;
```

(b) **Insert the post-journal last-moment validation** between
`journal::write_journal` and the `evict_present_device` call. Same
helper, called twice; the position is what changes the failure
semantics. A drift detected here preserves the journal for `braid
recover`, because the journal is already on disk and the error
short-circuits before `journal::clear_journal` further down.

```rust
journal::write_journal(params.paths, &journal)?;

// (Post-journal) last-moment safety gate: catch drift in the small
// window between the pre-journal probe and pool_balance_single.
// BtrfsBalanceSingle ships -f (cli/src/cmd.rs:553), which skips
// btrfs-progs' missing-device safety timeout (reference/btrfs-progs/
// cmds/balance.c:558-561). Without this gate, a disk going MISSING
// here could subject the pool to a dangerous profile conversion that
// can flip the fs read-only (balance.c:524-569). Failure here keeps
// the journal in place because we are below journal::write_journal
// and above journal::clear_journal -- standard "preserved for
// recover" semantics, same as today's evict_present_device errors.
// Remediation suffix points at recover because pending-op.json now
// exists; check_no_pending_operation (cli/src/preflight.rs:42-54)
// would block a re-run of `braid remove` and direct the user to
// recover anyway. Hot-unplug variant preserves the existing rich
// guidance from pool.rs:373-388 verbatim (matched by the existing
// test at pool.rs:1507-1554).
crate::pool::validate_pool_topology(
    runner,
    fs,
    &work_plan.mount_point,
    &work_plan.target_mapper.0,
    &work_plan.expected_present_identities,
)
.map_err(|drift| {
    let detail = drift.detail();
    let suffix = if drift.is_target_hot_unplug() {
        "cryptsetup reports `device: (null)` (hot-unplug). \
         Run `braid recover` to reconcile pool.json. \
         The broken mapper does not self-heal on replug; if \
         `cryptsetup status` still reports `device: (null)` after \
         recover, close + reopen the mappers (`braid lock` then \
         `braid unlock`, or reboot then `braid unlock`) before \
         retrying the remove."
    } else {
        "Run `braid recover` to reconcile."
    };
    RemoveError::Validation(format!("{detail}. {suffix}"))
})?;

evict_present_device(...);
```

(c) **Update the evict call** (`remove.rs:253`) to the slimmed
signature:

```rust
evict_present_device(
    runner,
    &work_plan.target_mapper.0,
    &work_plan.mount_point,
    work_plan.remaining == 1,
    params.progress,
)?;
```

`work_plan.remaining == 1` is the same expression the planner uses to
decide whether to push the `BtrfsBalanceSingle` step into the dry-run
preview at `remove.rs:131`, so the dry-run / journal / execute trio
becomes byte-faithful by construction. `work_plan.expected_present_identities`
(new field of type `BTreeMap<MapperName, DeviceIdentity>`,
populated at planning time from the planner's `pool.devices`)
feeds both validation calls and gives them *full device identity*,
not cardinality and not just mapper-set, so a same-count survivor
swap (one external removal + one external addition between plan
and execute) AND a same-mapper replacement (mapper name unchanged
but `devid` or `luks_uuid` differs) both fail validation rather
than slipping through with stale `check_eviction_space` capacity
assumptions (`remove.rs:397`).

Both validation calls map their `String` error to
`RemoveError::Validation` -- the journal-stay vs. clean-failure
distinction is positional (which side of `journal::write_journal` the
call lives on), not type-based. This mirrors how
`membership::save_membership` and other post-journal errors propagate
today: any `?` between `journal::write_journal` and
`journal::clear_journal` preserves the journal for recover.

### 4. Recover-side hardening (`cli/src/recover.rs:962-981`)

Extend the existing `OpKind::Remove` guard in
`execute_generic_live_pool_recovery` so it preserves *every*
`pre_membership` disk that is still owned by btrfs (in
`pool.null_underlying` or `pool.missing_devids`), not just the
remove target. Without this, a non-target disk that went MISSING
between `journal::write_journal` and the post-journal validation
failure would be pruned from `pool.json` by recover, even though no
btrfs mutation occurred.

Concretely, replace the single-target restoration block with a loop
over `plan.journal.pre_membership.disks`:

```rust
if matches!(&plan.journal.op, journal::OpKind::Remove { .. }) {
    for (name, member) in &plan.journal.pre_membership.disks {
        if recovered.disks.contains_key(name) {
            continue;
        }
        let mapper = config::mapper_name(name);
        let in_null_underlying = pool
            .null_underlying
            .iter()
            .any(|n| n.mapper == mapper);
        let in_missing = member
            .devid
            .map(|d| pool.missing_devids.contains(&d))
            .unwrap_or(false);
        if in_null_underlying || in_missing {
            recovered.disks.insert(name.clone(), member.clone());
        }
    }
}
```

The loop subsumes today's target-only restoration: the target is in
`pre_membership`, so it gets the same treatment as any other disk.
Update the surrounding code comment from "restore the target from
pre_membership" to "restore any pre_membership disk that btrfs still
owns".

This is the only `OpKind::Remove`-specific hardening this plan
requires. Other `OpKind` variants are unaffected.

### 5. Test updates

(a) **Helper-level test simplification** (`cli/src/pool.rs:1296-1382`).
The existing `evict_present_device_close_failure_emits_warn_row` test
(`pool.rs:1409`) and its `EvictRunner` mock (`pool.rs:1296-1382`)
mocked `BtrfsFilesystemShow`, `CryptsetupStatus`, and
`CryptsetupLuksUuid` purely to satisfy the in-helper probe. With the
probe gone, those arms become dead. Update:

  - Strip the three probe arms from `EvictRunner::run`, leaving only
    `BtrfsDeviceRemove` and `CryptsetupClose`.
  - Update the test's call to `evict_present_device` to the slimmed
    signature: drop the `fs` argument, pass `needs_balance: false`
    (the original 3-disk fixture had `remaining == 2`, so no balance
    was issued under the old probe path either).

(b) **New helper-level tests for `validate_pool_topology`** in the
`cli/src/pool.rs` test module. One per drift direction; each asserts
the function returns `Err(TopologyDrift)` with the expected discrete
fields (`target_present`, `target_null_underlying`,
`is_target_hot_unplug()`, observed sets/counts), and that no commands
beyond the probe queries are issued:

  - **target absent (plain)**: probe reports a pool whose devices
    no longer include the target mapper, and target is NOT in
    `pool.null_underlying`. Assertion: `target_present == false`,
    `target_null_underlying == false`, `is_target_hot_unplug() == false`.
  - **target absent (hot-unplug)**: probe reports target absent
    AND target is in `pool.null_underlying`. Assertion:
    `is_target_hot_unplug() == true` and `detail()` mentions
    "null-underlying / hot-unplug". This is the helper-side mirror
    of today's `evict_present_device_target_null_underlying_classifies_hot_unplug`
    (`pool.rs:1507-1554`); after this plan lands, that test no
    longer makes sense at the helper layer (the helper does not
    classify), but its richer command-level descendants live in
    section (g) below.
  - **non-target gone MISSING**: probe reports `missing_count > 0`
    with target still present and observed mappers a strict subset
    of expected.
  - **device count grew**: probe reports an additional mapper
    beyond `expected_present_identities` (e.g. a 4th disk
    auto-unlocked). Map equality flips on the extra key even
    though target is present and `missing_count == 0`.
  - **same-mapper replacement**: probe reports the same mapper set
    as the planner snapshot, but one mapper's `devid` or
    `luks_uuid` differs (operator ran `cryptsetup close` +
    `cryptsetup open` on a different LUKS device under the same
    `braid-<name>` between plan and execute, or a flapping disk
    was replaced under the same mapper name). Map equality flips
    on the value difference at that key. Assertion: the resulting
    `TopologyDrift` exposes the differing identity at the call
    site for diagnostic surfacing.
  - **same-count survivor swap**: probe reports the same number of
    mappers as expected, but with one expected mapper missing and
    one unexpected mapper present. This is the regression for the
    cardinality-only loophole the count-based check would have
    missed. Target is still present; `missing_count == 0`; only the
    set comparison flips.
  - **probe failed**: simulate `probe_pool` returning `Err`.
    `TopologyDrift.probe_error` is `Some(_)`; `detail()` says
    "topology validation probe failed: ...".
  - **happy path**: probe matches the expected mapper set exactly
    with `missing_count == 0`; helper returns `Ok(())`.

(c) **New command-level regression test #1: pre-journal drift**
in `remove.rs`'s test module. Pins the clean-failure contract for
drift detected before the journal write. Setup mirrors
`cmd_remove_prunes_acked_stats_for_removed_devid` (`remove.rs:766`)
for membership/config. Runner is a new stateful variant of
`RecordingRunner` whose `BtrfsFilesystemShow` returns 3 devices on
the first call (planning probe) and 2 devices on the second call
(pre-journal validation probe), with the target mapper present in
both. The third call is never reached.

Assertions:

```rust
let result = cmd_remove(...);
assert!(result.is_err(), "drift must fail before mutation");

let calls = log.lock().unwrap();
assert!(
    !calls.iter().any(|c| matches!(
        c,
        CmdRequest::BtrfsBalanceSingle { .. }
            | CmdRequest::BtrfsDeviceRemove { .. }
            | CmdRequest::CryptsetupClose { .. },
    )),
    "validation must reject drift before any mutation; calls: {calls:?}",
);

// Pre-journal failure: NO pending-op.json (principle 3,
// `docs/principles.md:23`). Idiom matches the inverse assertion at
// `remove.rs:853` in `journal_survives_evict_failure`.
assert!(
    journal::load_journal(&paths).unwrap().is_none(),
    "pre-journal validation failure must NOT leave a pending-op.json",
);

// Error message must direct the user to re-run `braid remove`, not
// to recover -- pre-journal failures are "command never started".
let err = result.unwrap_err();
let msg = format!("{err}");
assert!(
    msg.contains("re-run `braid remove`"),
    "pre-journal drift error must direct user to re-run remove; got: {msg}",
);
```

Preamble (Intent / Why / Scenario):

  - **Intent**: a `cmd_remove` whose pre-journal validation probe
    sees a drifted topology fails fast with no mutation AND no
    `pending-op.json`.
  - **Why**: pins (a) ADR 022 -- a regression that reintroduces
    `pool.devices.len()` derivation in `evict_present_device` flips
    the no-mutation assertion; (b) principle 3 -- a regression that
    moves validation below `journal::write_journal` (e.g. back
    inside `evict_present_device`) flips the `pending-op.json`
    assertion; (c) drift detection -- a regression that drops the
    topology-match check flips `is_err()`.
  - **Scenario**: while the user paused at the `yes` confirmation
    prompt, a third disk went MISSING. The planner would have
    rejected this at `check_no_missing_devices` (`remove.rs:362`);
    execution-time `validate_pool_topology` enforces the same
    invariant *before* the journal write.

(d) **New command-level regression test #2: post-journal drift**
in the same test module. Pins the journal-preserved contract for
drift detected in the small window between pre-journal validation
and `pool_balance_single`. Same `RecordingRunner` shape, but the
stateful mock returns 3 devices on the first call (planning probe),
3 devices on the second call (pre-journal validation probe -- now
passes), and 2 devices on the third call (post-journal validation
probe -- fails). The target mapper is present in all three responses.

Assertions:

```rust
let result = cmd_remove(...);
assert!(result.is_err(), "post-journal drift must fail before mutation");

let calls = log.lock().unwrap();
assert!(
    !calls.iter().any(|c| matches!(
        c,
        CmdRequest::BtrfsBalanceSingle { .. }
            | CmdRequest::BtrfsDeviceRemove { .. }
            | CmdRequest::CryptsetupClose { .. },
    )),
    "post-journal validation must reject drift before issuing -f balance \
     or any other mutation; calls: {calls:?}",
);

// Post-journal failure: pending-op.json MUST survive for recover --
// the journal was already written before the drift was detected.
// Same idiom as `journal_survives_evict_failure` at remove.rs:853.
assert!(
    journal::load_journal(&paths).unwrap().is_some(),
    "post-journal validation failure must preserve pending-op.json so \
     braid recover can reconcile",
);

// Error message must direct the user to recover, not to re-run remove
// -- check_no_pending_operation would block a re-run anyway.
let err = result.unwrap_err();
let msg = format!("{err}");
assert!(
    msg.contains("`braid recover`"),
    "post-journal drift error must direct user to recover; got: {msg}",
);
assert!(
    !msg.contains("re-run `braid remove`"),
    "post-journal drift error must NOT direct user to re-run remove \
     (check_no_pending_operation would block it); got: {msg}",
);
```

Preamble (Intent / Why / Scenario):

  - **Intent**: drift detected after `journal::write_journal` and
    before any mutation fails the command, runs zero mutation
    commands (critically, no `BtrfsBalanceSingle -f`), and leaves
    the journal on disk for `braid recover`.
  - **Why**: pins the post-journal safety gate. A regression that
    drops the post-journal validation flips the no-mutation
    assertion (because `pool_balance_single` would proceed against
    the drifted pool with `-f` skipping btrfs's missing-device
    timeout, balance.c:558-561). A regression that mis-orders the
    seam (validation after `journal::clear_journal`, or after a
    successful evict) is structurally caught by the
    journal-preserved + no-mutation pair.
  - **Scenario**: pre-journal validation just passed; in the
    microseconds before `pool_balance_single` issues, a previously
    flapping disk went MISSING (or was hot-unplugged). With the
    post-journal gate, the dangerous balance never starts; without
    it, btrfs's `--force` timeout-skip would let it proceed.

(e) **New recover-side unit test** in `cli/src/recover.rs`'s test
module pinning the broadened `OpKind::Remove` guard:

  - **non-target MISSING preserved on recover**: set up
    `pre_membership = {disk1, disk2, disk3}`, journal
    `OpKind::Remove { name: "disk2" }`, and a mocked
    `PoolState` where `pool.devices = [disk1, disk2]`,
    `pool.missing_devids = [<disk3's devid>]`. Run the recover
    membership reconstruction. Assert the recovered membership
    contains `disk1`, `disk2`, AND `disk3` (not just the target).
  - **non-target null-underlying preserved on recover**: same shape
    but with `pool.null_underlying` populated for disk3 instead.
  - **non-target genuinely gone is NOT preserved**: regression for
    the unrelated case where a disk *was* successfully removed
    (e.g. via a different prior op). With `disk3` neither in
    `pool.devices`, `pool.missing_devids`, nor `pool.null_underlying`,
    recover does not resurrect it.

(f) **New end-to-end command-level test** in `cli/src/remove.rs`'s
test module, composing post-journal validation failure with a
recover invocation, per the reviewer's regression spec:

  - Setup mirrors regression test #2: planning probe = 3 devices,
    pre-journal probe = 3 devices, post-journal probe shows disk3
    gone (`pool.missing_count == 1`, target disk2 still present).
  - Assert `cmd_remove` returns `Err`, `pending-op.json` survives,
    and the error directs the user to recover.
  - Then drive `cmd_recover` with a probe whose state still reports
    `pool.missing_devids` containing disk3's devid.
  - Assert the resulting `pool.json` membership contains disk1,
    disk2, AND disk3 (the non-target MISSING disk is preserved).
  - Assert `pending-op.json` is cleared after recover succeeds.

  This is the test the reviewer specifically requested. It pins the
  end-to-end contract: post-journal drift -> journal preserved ->
  recover preserves non-target MISSING -> pool.json is faithful.

(g) The existing
`remove_two_disk_pool_balances_single_before_device_remove`
(`remove.rs:804`) and
`two_to_one_remove_invokes_survivor_capacity_preflight`
(`remove.rs:721`) tests continue to pass: with `work_plan.remaining
== 1` on a stable 2-disk pool, the planner passes `needs_balance ==
true`, both validation probes match the planned topology, and
execution still issues `BtrfsBalanceSingle` before
`BtrfsDeviceRemove`. They protect the inverse direction (balance IS
issued when planner said so).

(h) The existing `journal_survives_evict_failure`
(`remove.rs:835-889`) test continues to pass: with both
validation probes matching the planned topology, execution proceeds
to `pool_remove_device`, which fails (per
`RecordingRunner::with_device_remove_failure`), and the journal
survives the error exit. The test was already protecting the same
"errors past `journal::write_journal` preserve `pending-op.json`"
contract that our new post-journal regression test relies on, so
the two are mutually reinforcing.

(i) **New same-count survivor-swap regression test** in
`cli/src/remove.rs`'s test module. Setup: 3-disk pool
{disk1, disk2, disk3}, plan removal of disk2. Stateful runner
returns 3 devices on the first call (planning probe -- planner
captures `expected_present_identities = {braid-disk1: id1,
braid-disk2: id2, braid-disk3: id3}`), and 3 devices on the
second call (pre-journal validation probe), but the second
response substitutes braid-disk4 for braid-disk3:
`pool.devices = [braid-disk1, braid-disk2, braid-disk4]`,
`pool.missing_count == 0`. Assertions: `Err`, no
`BtrfsBalanceSingle` / `BtrfsDeviceRemove` / `CryptsetupClose`,
`pending-op.json` does not exist, error message names "topology
changed" and includes both "braid-disk3" (expected key, observed
absent) and "braid-disk4" (observed key, expected absent). This
pins the mapper-set-drift case.

(i') **New same-mapper-replacement regression test** in the same
test module. Setup: 3-disk pool {disk1, disk2, disk3}, plan
removal of disk2. Stateful runner returns the same mapper set on
both calls, but disk3's `devid` (or `luks_uuid`) differs between
calls -- the planner saw `(braid-disk3, devid=3, luks_uuid=A)`,
the validation probe sees `(braid-disk3, devid=4, luks_uuid=B)`.
Assertions: `Err`, no mutation commands, `pending-op.json` does
not exist, error message includes "braid-disk3" and signals
identity drift (e.g. mentions devid or luks_uuid difference).
This pins the same-mapper-replacement case that
`BTreeSet<MapperName>` would have missed and that
`BTreeMap<MapperName, DeviceIdentity>` catches.

(j) **New target-hot-unplug regression tests in
`cli/src/remove.rs`'s test module** -- one pre-journal, one
post-journal. Each replaces the helper-level
`evict_present_device_target_null_underlying_classifies_hot_unplug`
(`pool.rs:1507-1554`) at the seam where the rich UX is now actually
emitted. The two tests pin *different* hot-unplug remediation
flows -- the message tokens diverge by call position because
`braid recover` is only meaningful when a journal exists.

  - **Pre-journal hot-unplug** (no journal yet, `braid recover`
    would fail with "no pending operation journal found"
    `recover.rs:1086-1094`). Assertions:
    * error message contains "braid-disk2", "device: (null)",
      "hot-unplug", "braid lock", "braid unlock", "reboot",
      "re-run `braid remove`", "remove did not start";
    * error message does NOT contain "braid recover" (would mislead
      the user into a command that fails);
    * no mutation commands recorded;
    * `pending-op.json` does not exist.
  - **Post-journal hot-unplug** (journal already on disk; recover
    is the right path). Assertions:
    * error message contains "braid-disk2", "device: (null)",
      "hot-unplug", "braid recover", "braid lock", "braid unlock",
      "reboot" (verbatim wording from the existing
      `pool.rs:373-388` fail-closed);
    * error message does NOT contain "re-run `braid remove`"
      (`check_no_pending_operation` would block the re-run);
    * no mutation commands recorded;
    * `pending-op.json` survives for `braid recover`.

After (j) lands, the helper-level test
`evict_present_device_target_null_underlying_classifies_hot_unplug`
should be deleted -- the helper no longer classifies; (j)
exercises the same UX at its new home. The deletion is part of
this plan's `cli/src/pool.rs` test-module trim.

### Out of scope

- The "best-effort cryptsetup close + warn-row" pattern at
  `pool.rs:430-462` and `replace.rs:706-748` is a near-duplicate.
  Worth extracting next to `close_mapper_with_retry` in
  `cli/src/mapper_close.rs:21`, but as its own commit -- it has its
  own design surface (color-flag ownership, replace.rs's trailing
  "Old device closed. If repurposing..." line at `replace.rs:730-732`)
  and shouldn't enlarge the blast radius of this remove-side fix.

## Files to modify

- `cli/src/types.rs` -- extend `MapperName`'s derive list (line 13)
  from `Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`
  to also include `PartialOrd, Ord`, so `MapperName` works as a
  `BTreeMap` key. `LuksUuid` (line 9) already has the derives
  needed for `DeviceIdentity` (`Eq, Clone`), so no change there.
  Other newtypes in this file (`ByIdPath`, `MountPoint`) are not
  touched.
- `cli/src/pool.rs` -- new `validate_pool_topology` function plus
  `TopologyDrift` and `DeviceIdentity` structs;
  `is_target_hot_unplug()` and `detail()` methods on
  `TopologyDrift`. Slim helper signature (drop `fs`, drop probe,
  add `needs_balance`); doc comment rewritten (drop "Probes the
  current pool..." line at `:345` and the entire fail-closed
  paragraph at `:352-359`); fail-closed/in-pool block at `:371-388`
  deleted; remaining/balance branch updated; `EvictRunner` test
  fixture trimmed (`pool.rs:343-465`, `pool.rs:1296-1382`). Delete
  the helper-level test
  `evict_present_device_target_null_underlying_classifies_hot_unplug`
  (`pool.rs:1507-1554`) -- its UX is moved to command-level tests
  (j) below, where the rich error is now actually emitted. Add new
  helper-level validation tests for `validate_pool_topology`
  including same-count swap, hot-unplug, and probe-failed cases.
- `cli/src/remove.rs` -- add `expected_present_identities:
  BTreeMap<MapperName, DeviceIdentity>` field on `RemoveWorkPlan`
  (`remove.rs:94-126`); populate it at planning time from
  `pool.devices` (each entry is `(d.mapper.clone(),
  DeviceIdentity { devid: d.devid, luks_uuid:
  d.luks_uuid.clone() })`). Insert TWO `validate_pool_topology` calls in
  `RemovePlan::execute`: one between sleep-inhibitor acquisition
  (`:228-235`) and the journal block (`:237-250`) -- the
  pre-journal clean-failure gate, with the rich pre-journal
  remediation including a target-hot-unplug branch; one between
  `journal::write_journal` (`:249`) and the `evict_present_device`
  call (`:253-259`) -- the post-journal last-moment safety gate, with
  the rich post-journal remediation that preserves the existing
  `pool.rs:373-388` hot-unplug wording. Update the
  `evict_present_device` call at `:253` to the slimmed signature.
  Add new command-level tests in the test module: pre-journal
  drift, post-journal drift, end-to-end
  post-journal-then-recover, same-count survivor-swap regression,
  same-mapper-replacement regression, pre-journal
  target-hot-unplug, post-journal target-hot-unplug.
- `cli/src/recover.rs` -- broaden the `OpKind::Remove` guard at
  `:962-981` from "restore the target only" to "restore any
  pre_membership disk still owned by btrfs" (loop over
  `plan.journal.pre_membership.disks`, check each against
  `pool.null_underlying` / `pool.missing_devids`). Update the
  surrounding code comment to match. Add three new recover-side
  unit tests (non-target MISSING preserved, non-target
  null-underlying preserved, non-target genuinely gone is not
  resurrected).

No journal-format change. No public CLI surface change. Principle 3
and ADR 022 already mandate this contract; only the helper's own
doc comment and the planner-execute coupling change.

## Verification

1. **Unit tests**: `just test-rust`
    - The updated `evict_present_device_close_failure_emits_warn_row`
      passes with the slimmed signature and trimmed `EvictRunner`.
    - The new `validate_pool_topology` helper-level tests pass: each
      drift direction returns `Err` with a topology-aware message;
      the happy path returns `Ok(())`.
    - **Pre-journal drift regression test** passes: 3-device plan +
      2-device pre-journal validation probe -> `Err` returned, zero
      mutation commands recorded, `pending-op.json` does not exist
      after the failure (clean command failure), AND error message
      directs the user to "re-run `braid remove`".
    - **Post-journal drift regression test** passes: 3-device plan +
      3-device pre-journal probe (passes) + 2-device post-journal
      probe -> `Err` returned, zero mutation commands recorded
      (critically, no `BtrfsBalanceSingle -f`), `pending-op.json`
      DOES survive (preserved for `braid recover`), AND error
      message directs the user to "`braid recover`" (not to re-run
      remove, which would be blocked by `check_no_pending_operation`).
    - **End-to-end post-journal-then-recover regression test**
      passes: post-journal drift produces `Err` and a preserved
      journal; subsequent `cmd_recover` against a state that still
      reports the non-target disk as MISSING produces a `pool.json`
      containing all pre_membership disks (target + non-target
      MISSING), and clears `pending-op.json`.
    - **Recover-side unit tests** pass: non-target MISSING preserved
      via `pool.missing_devids`; non-target null-underlying
      preserved via `pool.null_underlying`; non-target genuinely
      gone (neither in `devices` nor missing/null-underlying) is
      not resurrected.
    - **Same-count survivor-swap regression test** passes: planning
      probe sees {disk1, disk2, disk3}; pre-journal probe sees
      {disk1, disk2, disk4} (3 devices, target still present,
      `missing_count == 0`). Validation flags the mapper-set
      mismatch, command returns `Err`, no mutation, no
      `pending-op.json`.
    - **Same-mapper-replacement regression test** passes: planning
      probe and pre-journal probe report the same mapper set, but
      one survivor's `devid` (or `luks_uuid`) differs between the
      two probes. `BTreeMap` value-equality flips on the identity
      change; command returns `Err`, no mutation, no
      `pending-op.json`. This pins the case that
      `BTreeSet<MapperName>` would have missed.
    - **Pre-journal target-hot-unplug regression test** passes:
      target absent + `pool.null_underlying` includes target.
      Error message contains journal-free remediation tokens
      (`device: (null)`, `hot-unplug`, `braid lock`, `braid unlock`,
      `reboot`, "re-run `braid remove`", "remove did not start")
      and explicitly does NOT contain "braid recover" (the
      pre-journal path has no journal, and `braid recover` would
      fail at `recover.rs:1086-1094`).
    - **Post-journal target-hot-unplug regression test** passes:
      same target-absent / null-underlying state, but at the
      post-journal seam. Error message contains the verbatim
      `pool.rs:373-388` wording -- `device: (null)`, `hot-unplug`,
      `braid recover`, `braid lock`, `braid unlock`, `reboot` --
      and does NOT contain "re-run `braid remove`"
      (`check_no_pending_operation` would block it).
      `pending-op.json` survives.
    - `remove_two_disk_pool_balances_single_before_device_remove`
      (`remove.rs:804`) and
      `two_to_one_remove_invokes_survivor_capacity_preflight`
      (`remove.rs:721`) continue to pass (planner-driven
      `needs_balance == true` on stable 2->1).
    - `cmd_remove_prunes_acked_stats_for_removed_devid`
      (`remove.rs:766`) continues to pass (no command-sequence or
      journal-state assertions; only acked-state cleanup).

2. **VM tests**: `just test-vm` for any check that exercises
   `braid remove` end-to-end. Both the 2-survivors-go-to-1 (balance
   issued) and N>=3-survivors-go-to-N-1 (no balance) cases should
   continue to behave correctly against a non-drifting pool.

3. **Build**: `cargo build` -- the helper drops `fs` and
   `Filesystem`, but `validate_pool_topology` and `maybe_restore_raid1`
   still consume them, so the
   `use crate::probe::{Filesystem, probe_pool}` import at
   `pool.rs:2` stays unchanged. The new `MapperName` `PartialOrd,
   Ord` derives in `cli/src/types.rs` are required for
   `BTreeMap<MapperName, DeviceIdentity>` to compile; without
   them, the `validate_pool_topology` definition and every call
   site that constructs the expected-identities map would fail to
   build. `cargo build` and `just test-rust` cover this.
