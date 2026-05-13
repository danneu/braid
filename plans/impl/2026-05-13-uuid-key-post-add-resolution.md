# Plan: UUID-key the post-add resolution boundary in `braid add` / `braid recover`

## Context

Decision 024 § "Runtime Handles And Labels" rule 6 (`docs/decisions/024-luks-uuid-identity.md:90-118`)
says code must not parse mapper names or LUKS labels to correlate live pool
state. The existing-pool branch of `add` violates this rule in **two**
adjacent places that operate on the same `pool_after` -- and the same
violation recurs in the recovery replay path:

1. **Per-target ack-cleanup devid lookup** (`cli/src/add.rs:1214-1220`,
   using the shared `pub(crate) fn devid_for_mapper_path` at
   `cli/src/add.rs:724-736`):

   ```rust
   let devid =
       devid_for_mapper_path(&pool_after, &target.mapper_path).ok_or_else(|| {
           AddError::AckCleanupFailed {
               stage: "post-add probe",
               detail: format!("{}: not found in pool after add", target.mapper_path),
           }
       })?;
   ```

   This runs **first**, inside the per-target loop, immediately after each
   `btrfs device add`. A drifted live mapper would `ok_or_else` and fail
   here -- *before* the later sanity loop ever ran.

2. **Final sanity loop** (`cli/src/add.rs:1232-1245`):

   ```rust
   for target in journal_targets.iter().map(|(_, t)| t) {
       let mapper = mapper_name(target.name.as_str());
       if !pool_after.devices.iter().any(|d| d.mapper == mapper) {
           return Err(AddError::Validation(format!(
               "disk '{}' was not found in the live pool after add",
               mapper
           )));
       }
   }
   ```

   `journal_targets` is `LuksUuidMap<AddJournalTarget>` (UUID is the map key)
   yet the predicate is mapper-equality. The error body also leaks the
   `braid-<name>` mapper instead of the operator's `DiskName`.

3. **Recovery add-replay devid lookup** (`cli/src/recover.rs:2539-2544`
   and the post-replay target sweep at `cli/src/recover.rs:2606-2615`):

   ```rust
   let devid = devid_for_mapper_path(&pool, &mapper_path).ok_or_else(|| ...)?;
   ```

   Same shared helper, same mapper-keyed correlation, same drift hazard
   when recovery resumes after a mapper has been reopened under a drifted
   name.

History: the original guard was added pre-UUID-identity in commit `982bc6f8`
(May 5). Commit `3eba9ab1` (May 12, "wip: migrate add to UUID identity
phase 3b") moved the *iteration source* in the final loop to the UUID-keyed
journal map but left every predicate -- ack-cleanup, final sanity, recovery
replay -- as mapper-equality. This is the classic half-migrated leftover.

The bug is currently latent because braid still controls mapper naming for
its own opens, so mapper-name and UUID agree in practice. It becomes a real
bug the moment any path lets the live mapper diverge from
`mapper_name(target.name)`: operator interference, a returned-disk
adoption whose recovered live mapper drifted, or a future code path that
opens via a derived alternate mapper. Decision 024 designs braid to tolerate
that drift; these guards currently don't.

## Approach

Collapse the three mapper-keyed correlations onto a single UUID-keyed
post-add resolver, and carry enough identity through `PoolAddExecutionTarget`
to drive it.

### Change 1 -- replace the shared mapper-keyed helper

In `cli/src/add.rs`, replace
`pub(crate) fn devid_for_mapper_path(pool: &PoolState, mapper_path: &str) -> Option<u64>`
(lines 724-736) with a UUID-keyed resolver returning the matched device so
each call site can extract the field it needs:

```rust
/// Single UUID-keyed post-add resolution boundary for `cmd_add` and
/// `cmd_recover`'s add-replay arm. Returns the live PoolDevice whose
/// LUKS UUID matches `uuid`. Callers extract `.devid` for ack-cleanup
/// or treat `Some(_)` as proof that the journaled target is in the pool.
/// UUID-keyed per decision 024 so mapper/label drift between open and
/// probe does not break presence detection.
pub(crate) fn find_added_device_by_uuid<'a>(
    pool: &'a PoolState,
    uuid: &LuksUuid,
) -> Option<&'a PoolDevice> {
    pool.devices.iter().find(|d| d.luks_uuid == *uuid)
}
```

Delete the old helper and its existing unit test
(`devid_for_mapper_path_matches_mapper_name`, `cli/src/add.rs:2354-2387`);
both pin the wrong contract.

### Change 2 -- carry UUID + DiskName in `PoolAddExecutionTarget`

`PoolAddExecutionTarget` (`cli/src/add.rs:277-280`) currently has only
`mapper_path` and `force`. Grow it to:

```rust
struct PoolAddExecutionTarget {
    mapper_path: String,
    force: bool,
    luks_uuid: LuksUuid,
    name: DiskName,
}
```

`mapper_path` stays -- it is the legitimate `btrfs device add` argv. The
UUID and DiskName are added for the post-add resolution boundary and
operator-facing error messaging.

Update the three push sites; UUID + name are already in scope at each:

- `cli/src/add.rs:883` (OpenRecoverable arm): `target.luks_uuid`, `target.name`.
- `cli/src/add.rs:949` (BraidLabeledRecoverable post-FSID-verify arm):
  `verified.luks_uuid`, `verified.name`.
- `cli/src/add.rs:1107` (Pass 2 Fresh arm): `target.luks_uuid`, `target.name`.

### Change 3 -- UUID-key the existing-pool add per-target loop and final guard

In `cli/src/add.rs:1203-1245`, both the per-target ack-cleanup boundary
and the final pre-membership sanity loop become UUID-keyed. The two
boundaries observe `pool_after` at **different times**, so both stay --
only the mapper-name reconstruction goes away.

```rust
} else {
    // Add each to existing pool
    for target in &needs_pool_add {
        pool_add_device(runner, &target.mapper_path, mount_point, target.force)?;
        eprintln!("Device added to pool: {}", target.mapper_path);
        let pool_after = probe_pool(runner, fs, mount_point).map_err(|e| {
            AddError::AckCleanupFailed {
                stage: "post-add probe",
                detail: format!("{}: {e}", target.name),
            }
        })?;
        // UUID-keyed per decision 024: tolerates mapper/label drift between
        // Pass 2's open and this probe. The found PoolDevice is the proof
        // that the journaled target was in the live pool at the time of
        // this per-target probe; .devid feeds the acked-stats sweep.
        // Preserved error shape (AckCleanupFailed / "post-add probe") so
        // the existing fail-closed expectation is unchanged -- the only
        // difference is the correlation key.
        let dev = find_added_device_by_uuid(&pool_after, &target.luks_uuid)
            .ok_or_else(|| AddError::AckCleanupFailed {
                stage: "post-add probe",
                detail: format!("{}: not found in pool after add", target.name),
            })?;
        alert::drop_ghost_acked_for_devids(params.paths, &[dev.devid]).map_err(|e| {
            AddError::AckCleanupFailed {
                stage: "live-pool add",
                detail: format!("devid {}: {e}", dev.devid),
            }
        })?;
    }

    // Membership is committed by btrfs device add. Persist it before the
    // long post-add balance while leaving the journal in place so recovery
    // still knows the balance is owed if interrupted.
    let pool_after = probe_pool(runner, fs, mount_point)?;
    // Final pre-membership guard. Distinct time boundary from the
    // per-target loop above: a target that was live then could have
    // disappeared by now, and `enrich_from_pool_state` only iterates
    // `pool.devices` (it does not fail when a membership UUID is absent
    // from the live set -- see `cli/src/membership.rs:557-563`). Without
    // this loop, membership could be persisted for a target that is no
    // longer live. UUID-keyed per decision 024; error names the operator
    // DiskName, not the `braid-<name>` mapper.
    for (uuid, target) in journal_targets.iter() {
        if find_added_device_by_uuid(&pool_after, uuid).is_none() {
            return Err(AddError::Validation(format!(
                "disk '{}' was not found in the live pool after add",
                target.name
            )));
        }
    }
    let mut final_membership = journal.target_membership.clone();
    let _ = membership::enrich_from_pool_state(&mut final_membership, &pool_after)?;
    membership::save_membership(&final_membership, params.paths)?;
    // ... (post-add balance unchanged)
}
```

Key consequences:

- **Two distinct boundaries, both UUID-keyed.** The per-target loop
  proves presence *immediately after* each `btrfs device add` (the moment
  ack-cleanup must obtain a devid). The final loop proves presence
  *immediately before* membership save (the moment we are about to
  persist `pool.json`). They cover different time windows and the final
  one is not subsumed by the first.
- **Error-type fidelity preserved.** Per-target UUID-miss raises
  `AddError::AckCleanupFailed { stage: "post-add probe", detail: "<name>:
  not found in pool after add" }` -- same variant and stage the existing
  `cmd_add_post_add_probe_uncertainty_is_fatal` test pins
  (`cli/src/add.rs:4217-4277`). The final pre-membership UUID-miss raises
  `AddError::Validation(...)`, matching the prior shape of the removed
  mapper-keyed sanity loop. Only the correlation key and the formatted
  name change.
- **No leaked mapper strings.** Both error bodies now render `target.name`
  (the operator `DiskName`), not `braid-<name>`.

### Change 4 -- mirror in `cmd_recover`'s add-replay arm

The shared helper switch forces both `cmd_recover` call sites to update;
UUID is already in scope at both. Both also keep `target.name` for
operator-facing error bodies (which they already use correctly):

- `cli/src/recover.rs:2538-2544`: replace
  `devid_for_mapper_path(&pool, &mapper_path)` with
  `find_added_device_by_uuid(&pool, target_uuid)`. Continue extracting
  `.devid` for the existing `drop_ghost_acked_for_devids` call.
- `cli/src/recover.rs:2600-2616` (`sweep_recovered_add_acked_stats`):
  iterate `for (uuid, target) in targets` and resolve via
  `find_added_device_by_uuid(pool, uuid)`. Drop the
  `let mapper = config::mapper_name(target.name.as_str()); let mapper_path = format!(...);`
  reconstruction.

### Change 5 -- tests

All test preambles below follow the contiguous `// Intent / // Why it
exists / // Scenario` line-comment form required by `docs/testing.md`
("Preamble: literal `//` line-comment form").

**Helper-level unit test.** Drop the existing
`devid_for_mapper_path_matches_mapper_name` (`cli/src/add.rs:2354-2387`)
-- it pins the old contract. Replace it with:

```rust
// Intent: find_added_device_by_uuid resolves the live PoolDevice (and
// therefore its devid) by LUKS UUID, tolerating mapper drift.
// Why it exists: post-add ack-cleanup and presence verification must key
// on the persistent LUKS UUID per decision 024, not on a reconstructed
// `braid-<name>` mapper.
// Scenario: post-add probe reports the new device under a drifted mapper
// ("braid-WRONG") with the journaled UUID; resolver still matches.
#[test]
fn find_added_device_by_uuid_tolerates_drifted_mapper() { ... }
```

**Primary regression test (command level, `cmd_add`).** Pattern after
`cmd_add_post_add_probe_uncertainty_is_fatal` (`cli/src/add.rs:4230-4277`)
and `with_new_mapper_omitted_from_probe` (`cli/src/add.rs:3700-3703` and
the `pool_show` switch at `cli/src/add.rs:3748-3763`).

Add a new knob `AddFullPathRunner::with_added_mapper_drifted(rename: &str)`
that, in `pool_show()`, substitutes the freshly-added `braid-disk2`
mapper line with the supplied drifted name (e.g. `braid-WRONG`) while
keeping the same backing device path / LUKS-UUID mapping returned by the
underlying-device and `cryptsetup status` shims. Then:

```rust
// Intent: existing-pool add succeeds when the post-add probe reports the
// new device under a drifted mapper but the journaled LUKS UUID is
// present in the live pool.
// Why it exists: post-add membership correlation must be UUID-keyed per
// decision 024. A reverted-to-mapper-keyed implementation must fail this
// test even if helper-level unit tests still pass.
// Scenario: `braid add disk2=...` completes pool_add_device. The post-add
// probe reports the new mapper as `braid-WRONG` (with the correct LUKS
// UUID 2222...). Add must succeed through membership persistence and
// journal clear.
#[test]
fn cmd_add_succeeds_when_post_add_mapper_drifted() {
    let runner = AddFullPathRunner::live().with_added_mapper_drifted("braid-WRONG");
    // ... mirror cmd_add_post_add_probe_uncertainty_is_fatal harness setup
    let result = cmd_add(&runner, &fs, &AddParams { /* ... */ });
    assert!(result.is_ok(), "expected add to tolerate mapper drift, got {result:?}");
    // Assert membership was persisted and journal was cleared (existing
    // test helpers expose this; mirror cmd_add_post_add_probe_uncertainty_is_fatal).
}
```

This test fails under the current code at the per-target ack-cleanup site
(`devid_for_mapper_path(&pool_after, "/dev/mapper/braid-disk2")` returns
`None` -> `AckCleanupFailed`), so reverting that site to mapper-keyed
correlation re-breaks it. The final pre-membership guard is exercised
through this same end-to-end path because the existing
`AddFullPathRunner::pool_show()` returns the same drifted mapper on every
probe; a regression that re-introduced a mapper-keyed final guard would
also break this test.

**Recovery regression tests (command level, `execute_add_pool_mutation_recovery`).**
The shared helper switch changes both `cmd_recover` add-replay call sites,
so each needs an analogous drift-tolerance test. Pattern after the
existing reused-devid tests at `cli/src/recover.rs:5641-5688`
(`live_add_recovery_drops_ghost_for_reused_devid_via_replay`) and
`cli/src/recover.rs:5690+` (`live_add_recovery_drops_ghost_for_committed_but_closed_target`).

Replay-loop coverage:

```rust
// Intent: live-add recovery's replay loop succeeds when the post-replay
// probe reports the replayed target under a drifted mapper but the
// journaled LUKS UUID is present in the live pool.
// Why it exists: recovery's ack-cleanup devid lookup must be UUID-keyed
// per decision 024. A reverted-to-mapper-keyed replay loop would crash
// recovery exactly when drift-tolerance matters most.
// Scenario: recovery replays `pool_add_device` for disk2; the post-replay
// probe reports it as `braid-WRONG` carrying the journaled UUID. The
// replay loop's UUID-keyed lookup resolves it and the reused-devid ghost
// is still dropped.
#[test]
fn live_add_recovery_drops_ghost_under_drifted_mapper_via_replay() {
    // Mirror live_add_recovery_drops_ghost_for_reused_devid_via_replay's
    // setup, but configure the post-replay probe stub to report
    // /dev/mapper/braid-disk2's PoolDevice with .mapper = "braid-WRONG"
    // while keeping .luks_uuid = the journaled UUID for disk2 and
    // .devid = 4. Assert recovery completes and the reused-devid ghost
    // is dropped (acked_stats key "4" absent post-recovery).
}
```

All-live sweep coverage:

```rust
// Intent: live-add recovery's all-live sweep succeeds when the live pool
// reports a journaled target under a drifted mapper but its UUID is
// present.
// Why it exists: `sweep_recovered_add_acked_stats` must resolve every
// journaled target by UUID, not by reconstructed `braid-<name>` mapper.
// Scenario: disk2 was added to btrfs at reused devid 4 before the crash,
// so recovery sees all targets live and skips the replay loop. The live
// pool reports disk2 under `braid-WRONG` carrying the journaled UUID;
// the sweep still drops the reused-devid ghost.
#[test]
fn live_add_recovery_drops_ghost_under_drifted_mapper_committed_but_closed() {
    // Mirror live_add_recovery_drops_ghost_for_committed_but_closed_target,
    // but the seed PoolState built by `pool_state_one_disk()`-equivalent
    // for the all-live path carries .mapper = "braid-WRONG" with the
    // journaled UUID. Assert ghost drop succeeds.
}
```

Both recovery tests fail under the current `devid_for_mapper_path`
behavior (mapper-equality miss -> `RecoverError::AckCleanupFailed`), so
they regression-pin the recover.rs change for finding F3.

### Out of scope (deliberate)

- **The "symmetric live-iteration anchor" at `cli/src/add.rs:565-580`.**
  Dry-run preview step generation; `mapper_path` there is legitimate argv,
  not a live-state correlation key.
- **Sibling mapper-keyed correlations elsewhere** -- `add.rs:187`
  (`classify_braid_disk_fsid` "already in pool" check), `replace.rs:731`,
  `replace.rs:1489` (`check_new_not_in_pool`). Real sibling instances of
  the same anti-pattern but in different code paths (pre-mutation
  classification and "already-in-pool" guards) with different call-site
  state. Worth a follow-up sweep; not bundled here.

## Critical files

- `cli/src/add.rs:277-280` -- grow `PoolAddExecutionTarget`.
- `cli/src/add.rs:724-736` -- replace `devid_for_mapper_path` with
  `find_added_device_by_uuid`.
- `cli/src/add.rs:883,949,1107` -- push sites for `PoolAddExecutionTarget`.
- `cli/src/add.rs:1203-1245` -- UUID-keyed per-target loop **and** UUID-keyed
  final pre-membership guard (both retained, mapper reconstruction removed).
- `cli/src/add.rs:2354-2387` -- replace old helper unit test.
- `cli/src/add.rs:3646+`,`3700-3703`,`3748-3763` -- add
  `with_added_mapper_drifted` to `AddFullPathRunner`.
- `cli/src/add.rs:4230+` -- add `cmd_add_succeeds_when_post_add_mapper_drifted`.
- `cli/src/recover.rs:2538-2544`,`2600-2616` -- switch both call sites
  to `find_added_device_by_uuid`.
- `cli/src/recover.rs:5641+` -- add
  `live_add_recovery_drops_ghost_under_drifted_mapper_via_replay` and
  `live_add_recovery_drops_ghost_under_drifted_mapper_committed_but_closed`,
  patterned after the existing reused-devid tests at the same anchor.
- `cli/src/membership.rs:557-563` -- referenced by the plan to justify
  the retained final guard (`enrich_from_pool_state` is foreign-fail-closed
  but membership-absent-tolerant).
- Reference for the canonical pattern: `cli/src/replace.rs:1500-1525`
  ("Pattern 4 site"), `cli/src/add.rs:1927`, `cli/src/remove.rs:546`.
- Reference for the rule: `docs/decisions/024-luks-uuid-identity.md:90-118`.
- Reference for test preamble form: `docs/testing.md` § "Preamble: literal
  `//` line-comment form".

## Verification

1. `just test-rust` -- new helper unit test passes; new
   `cmd_add_succeeds_when_post_add_mapper_drifted` passes; new
   `live_add_recovery_drops_ghost_under_drifted_mapper_via_replay` and
   `live_add_recovery_drops_ghost_under_drifted_mapper_committed_but_closed`
   pass; existing `cmd_add_post_add_probe_uncertainty_is_fatal` still
   passes (both arms: the `with_post_add_probe_failure` case fires the
   probe-failure `AckCleanupFailed`; the `with_new_mapper_omitted_from_probe`
   case still fires the per-target UUID-miss `AckCleanupFailed { stage:
   "post-add probe" }` because the omitted mapper means the UUID is
   absent too); existing recovery reused-devid tests still pass.
2. `cargo clippy -p braid-cli --all-targets -- -D warnings` -- no new
   warnings.
3. `just test-vm` -- existing-pool add VM tests
   (`tests/cli/braid-add-disk.py`,
   `tests/cli/braid-add-persists-before-balance.py`,
   `tests/cli/add-returned-disk-after-remove-missing.py`) still pass.
4. `just test-vm` -- recovery tests touching the add-replay arm still
   pass (any `tests/cli/recover-*` covering interrupted add).
