# Fail-closed `evict_present_device` and Remove-aware recovery

## Context

Two layers leak the same phantom-success class when a `braid remove` target
transitions to null-underlying or btrfs-MISSING during the operation.

**Layer 1: `evict_present_device` (`cli/src/pool.rs:354-368`).**
The helper silently early-returns `Ok(())` when its in-helper re-probe finds
the target mapper missing from `pool.devices`:

```rust
let in_pool = pool.devices.iter().any(|d| d.mapper.0 == mapper);
if !in_pool {
    return Ok(());
}
```

Its sole production caller, `cmd_remove::execute` (`cli/src/remove.rs:260`),
then writes `pool.json` (`save_membership`), clears the journal, and prints
`Done. Disk '...' removed from pool.` -- regardless of whether eviction
actually happened.

If a hot-unplug (cryptsetup `device: (null)`) or btrfs-MISSING transition
lands between `plan_remove`'s probe and the in-helper probe -- a millisecond
window with `--yes`, the entire confirm prompt without -- `pool.devices.iter().any(...)`
returns false (because `probe_pool` filters null-underlying mappers into
`pool.null_underlying` and excludes btrfs-MISSING entries from `pool.devices`,
see `cli/src/probe.rs:280-298` and `cli/src/parse/btrfs_filesystem_show.rs:114-125`),
the helper returns Ok, and the operator sees a phantom success while btrfs
still owns the device.

**Layer 2: generic live-pool recovery (`cli/src/recover.rs:951-996`).**
Once the operator runs `braid recover`, `execute_generic_live_pool_recovery`
calls `build_membership_from_live_pool` (`cli/src/recover.rs:1725-1763`),
which iterates `pool.devices` only. For an `OpKind::Remove` whose target
sits in `null_underlying` or `missing_devids` rather than `devices`, the
rebuilt membership EXCLUDES the target. `save_membership(&recovered, ...)`
then writes that excluded-target view to `pool.json`, `clear_journal`
runs (`cli/src/recover.rs:993`), and the recover-layer reproduces the
exact same phantom success.

A fix that only addresses Layer 1 is incomplete: the journal preserved by
the helper-level fail-closed return gets cleared by recover with the wrong
membership written out. Both layers must be patched together.

This plan converts both shortcuts to states that preserve `pre_membership`
when the target is detectable as null-underlying or MISSING. Layer 1 fails
closed so the operator runs `braid recover`. Layer 2 reads `pool.null_underlying`
and `pool.missing_devids` to keep the target in the recovered membership
when it has not actually been evicted.

## Recommended fix

### Layer 1 -- `evict_present_device` fail-closed

Replace the early-return at `cli/src/pool.rs:366-368` with a `PoolError::Failed`
that distinguishes hot-unplug (mapper present in `pool.null_underlying`) from
the catch-all "no longer present" case (which subsumes btrfs-MISSING and any
other absence reason). Both branches direct the operator to `braid recover`
as the next command -- it is the only mutating command allowed under
`pending-op.json` (`cli/src/preflight.rs:42-55`; `status` and `lock` are
the read-only / safe siblings). Pointing at `braid remove-missing` directly
would be wrong because `remove-missing` is itself blocked by
`check_no_pending_operation` (`cli/src/remove_missing.rs:332`); pointing
at `braid unlock` would be wrong for the same reason
(`cli/src/unlock.rs:173`).

The null-underlying branch additionally warns that the broken mapper does
not self-heal. Per `docs/real-world/sata-hot-unplug.md:51-77`, `cryptsetup
status` keeps reporting `device: (null)` after replug because the dm-crypt
target was bound to the original SCSI node. The mapper has to be closed
and reopened (`braid lock` + `braid unlock`, or reboot + unlock). But
because `unlock` is blocked while the journal exists, this sequence is a
*post-recover* follow-up, not the first step. The error message therefore
sequences it explicitly: run `braid recover` first to clear the journal
and reconcile `pool.json`; then, only if the mapper is still null, do
the lock + unlock (or reboot + unlock) cycle before retrying the remove.

### Layer 2 -- Remove-aware generic recovery

Inside `execute_generic_live_pool_recovery` (`cli/src/recover.rs:951-996`),
after `build_membership_from_live_pool` produces `recovered`, special-case
`OpKind::Remove`: if the journal target's mapper appears in
`pool.null_underlying`, OR the target's pre-membership devid appears in
`pool.missing_devids`, restore the target's `DiskMember` entry into
`recovered` from `journal.pre_membership`. The remove did not commit, so
`pool.json` must continue to track the device.

`replay_post_mutation`'s `OpKind::Remove` arm (`cli/src/recover.rs:1656-1668`)
stays as-is -- its no-op contract is correct under both the previous and
the corrected memberships. After recovery, the operator can either fix
the device situation and re-run `braid remove`, or run `braid remove-missing`
once the journal is cleared and the device is confirmed dead.

## Implementation

### 1. `cli/src/pool.rs` -- replace lines 366-368

```rust
if !in_pool {
    let null_underlying = pool
        .null_underlying
        .iter()
        .any(|n| n.mapper.0 == mapper);
    let detail = if null_underlying {
        "cryptsetup reports `device: (null)` (hot-unplug). \
         Run `braid recover` to reconcile pool.json. \
         The broken mapper does not self-heal on replug; if `cryptsetup status` \
         still reports `device: (null)` after recover, close + reopen the mappers \
         (`braid lock` then `braid unlock`, or reboot then `braid unlock`) before \
         retrying the remove."
    } else {
        "remove did not commit. Run `braid recover` to reconcile pool.json."
    };
    return Err(PoolError::Failed(format!(
        "target {mapper} is no longer present in pool: {detail}"
    )));
}
```

Notes:
- `PoolError::Failed(String)` matches every nearby command-result error
  in the same file.
- Strings use plain ASCII per AGENTS.md.
- Fail-closed return happens before any status-line emission (the
  `color_enabled` setup at `pool.rs:370` is below the `in_pool` block),
  so no dangling `[wait]` rows.

### 2. `cli/src/pool.rs:352` -- replace doc comment

Replace:

```rust
/// Returns `Ok(())` as a no-op if the target mapper is not present in the pool.
```

with:

```rust
/// Fail-closed: returns `PoolError::Failed` if the in-helper re-probe finds the
/// target mapper absent from `pool.devices` (hot-unplug or btrfs-MISSING
/// transition between `plan_remove` and here). The caller relies on this to
/// keep the journal on disk and `pool.json` un-rewritten so the next
/// `braid recover` reconciles from live state. Layer-2 recovery
/// (`execute_generic_live_pool_recovery` for `OpKind::Remove`) honors the
/// same null-underlying / MISSING detection to avoid dropping the target
/// from `pool.json`.
```

### 3. `cli/src/recover.rs` -- guard inside `execute_generic_live_pool_recovery`

After the existing `let recovered = build_membership_from_live_pool(...)?;`
line at `cli/src/recover.rs:959-960`, switch `recovered` to `let mut` and
add the Remove-specific guard:

```rust
let prior = membership::load_membership(params.paths).ok();
let mut recovered =
    build_membership_from_live_pool(&pool, &plan.union, prior.as_ref(), by_id_resolver)?;

// OpKind::Remove guard: an absent target may indicate an in-progress race
// (mapper went null-underlying or btrfs-MISSING between plan_remove and
// helper / between helper and recovery), not a completed eviction.
// build_membership_from_live_pool walks pool.devices only; restore the
// target from pre_membership when probe_pool's null_underlying or
// missing_devids signal that btrfs still owns it.
if let journal::OpKind::Remove { name } = &plan.journal.op {
    if !recovered.disks.contains_key(name) {
        if let Some(target_member) = plan.journal.pre_membership.disks.get(name) {
            let mapper = config::mapper_name(name);
            let in_null_underlying = pool
                .null_underlying
                .iter()
                .any(|n| n.mapper == mapper);
            let in_missing = target_member
                .devid
                .map(|d| pool.missing_devids.contains(&d))
                .unwrap_or(false);
            if in_null_underlying || in_missing {
                recovered
                    .disks
                    .insert(name.clone(), target_member.clone());
            }
        }
    }
}
```

Imports: `config::mapper_name` is already available in `recover.rs` via
existing `use` lines for `config` (used by `name_from_mapper`); double-check
during implementation and add a `use crate::config::mapper_name;` line if
not in scope. `journal::OpKind` is already in scope.

The `pre_membership.disks[name].devid` field is populated by `enrich_from_pool_state`
(`cli/src/membership.rs:190-203`) on every successful add/replace, so in
practice it is always Some. The `unwrap_or(false)` keeps the recovery
correct (still includes the target via the null-underlying check) when a
hand-edited or stale `pool.json` lacks the devid.

### 4. `cli/src/pool.rs` tests (under existing test module)

Parameterize the existing `EvictRunner` (currently at `cli/src/pool.rs:1272-1331`,
hardcoded to a 3-disk pool) so it can:
- omit one mapper from `BtrfsFilesystemShow` output (simulate btrfs-MISSING
  / outright absence), and
- emit the omitted mapper's path back into the show output while reporting
  `device: (null)` for it via `CryptsetupStatus` (simulate null-underlying).

Add invocation recording (e.g. `Arc<Mutex<Vec<&'static str>>>` of request
tags) so the new tests can assert no `BtrfsDeviceRemove` / `CryptsetupClose`
leaked through. Give new fields safe defaults (`target_present: true`,
`null_underlying_target: false`) so the existing
`evict_present_device_close_failure_emits_warn_row` test stays unchanged
beyond switching its constructor call to a `..EvictRunner::default()`-style
shorthand.

Add two new tests below the existing one. Use the literal `// Intent` /
`// Why it exists` / `// Scenario` line-comment preamble per
`docs/testing.md:11-22`:

- `evict_present_device_target_missing_returns_error_without_mutating`:
  target mapper omitted from `BtrfsFilesystemShow`. Asserts:
  - `Err` result naming the target mapper, mentioning "no longer present
    in pool", and "remove did not commit" / "braid recover".
  - Recorded invocations contain neither `BtrfsDeviceRemove` nor
    `CryptsetupClose`.
- `evict_present_device_target_null_underlying_classifies_hot_unplug`:
  target mapper present in `BtrfsFilesystemShow` output but
  `CryptsetupStatus` returns `device: (null)`. Asserts:
  - Error classifies as hot-unplug (`device: (null)`).
  - Error tells the operator to run `braid recover` first (the
    immediate next command).
  - Error mentions `braid lock` + `braid unlock` (or reboot +
    `braid unlock`) as the post-recover follow-up if the mapper
    is still null.

Both tests reuse `RestoreFs` and `mp()` (already in the test module at
`cli/src/pool.rs:953` / `:533`).

### 5. `cli/src/recover.rs` tests

Add command-level `cmd_recover` tests in the existing `recover.rs` test
module (`cli/src/recover.rs` test module starts around line 3260) using
the same `// Intent` / `// Why it exists` / `// Scenario` preamble form.
Pattern after existing `OpKind::Remove`-flavored recover tests at
`cli/src/recover.rs:11842` and `cli/src/recover.rs:10823` (callers of
`OpKind::Remove`-built journals).

Two new tests:

- `cmd_recover_remove_with_null_underlying_target_preserves_membership`:
  - Pre-state: `pre_membership` has `disk-2` (devid 2), `target_membership`
    omits it; `pending-op.json` exists with `OpKind::Remove { name: "disk-2" }`.
  - Live pool: `pool.devices` excludes `disk-2`; `pool.null_underlying`
    contains `MapperName("braid-disk-2")` with `devid: 2`.
  - Run `cmd_recover`. Assert `pool.json` after recover contains
    `disk-2`, journal is cleared, exit Ok.
- `cmd_recover_remove_with_missing_target_preserves_membership`:
  - Pre-state same as above; `pool.missing_devids` contains `2` (no
    `null_underlying` entry).
  - Run `cmd_recover`. Assert `pool.json` after recover contains
    `disk-2`, journal is cleared.

Add a third regression test asserting the existing happy path
(target genuinely evicted) still drops the disk:

- `cmd_recover_remove_with_genuinely_evicted_target_drops_membership`:
  - Pre-state same; live pool has neither `disk-2` in `pool.devices`,
    `pool.null_underlying`, nor `pool.missing_devids`.
  - Run `cmd_recover`. Assert `pool.json` excludes `disk-2`, journal
    cleared.

## Files modified

- `cli/src/pool.rs` -- doc comment (line 352), shortcut body (lines
  366-368), `EvictRunner` (lines 1272-1331), new helper-level tests
  below line 1362.
- `cli/src/recover.rs` -- guard inside `execute_generic_live_pool_recovery`
  (immediately after line 960), new command-level tests in the existing
  test module.

No changes to `remove.rs`, `replace.rs`, `types.rs`, or `probe.rs`.
`RemoveError::Pool` (`#[from]` on `cli/src/remove.rs:35-36`) already
propagates the new error variant cleanly.

## Verification

```
just test-rust
```

Pass conditions:
- existing `evict_present_device_close_failure_emits_warn_row` passes
  unchanged.
- two new helper-level tests
  (`evict_present_device_target_missing_returns_error_without_mutating`,
  `evict_present_device_target_null_underlying_classifies_hot_unplug`)
  pass.
- three new recover tests pass: null-underlying preserve, missing
  preserve, genuine-eviction drop.

```
just test-vm
```

Existing remove / recover VM tests (e.g. any `cli-recover-*` and
remove-suite tests registered in `flake.nix`) must keep passing.

No new VM test is justified for the fail-closed branches: the race
windows are hard to reproduce deterministically in a VM and the unit
tests pin the contract precisely at the function and command boundaries.

Manual smoke (optional, post-implementation): assemble a 3-disk pool in
the VM, start `braid remove --yes <name>`, then force one mapper to
null-underlying via `dmsetup` mid-operation. Expected: `braid remove`
exits non-zero, `pool.json` is unmodified, journal still on disk;
subsequent `braid recover` preserves the target in `pool.json` and
clears the journal. Skip if unit tests suffice -- this is defensive,
not required.
