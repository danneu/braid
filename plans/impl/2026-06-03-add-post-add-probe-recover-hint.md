# Fix: post-add probe failure points the operator at `braid recover`, not acked-stats deletion

## Context

In the live-pool branch of `braid add` (`cli/src/add.rs#AddPlan::execute`), `btrfs
device add` commits pool membership, then braid probes the pool to confirm the new
device and resolve its devid. If that **post-add probe fails** (transient btrfs/I-O
fault) or the freshly added device **is not yet visible** in the probe result, braid
fails closed -- correctly, because it cannot prove which devid was assigned. But it
returns `AddError::AckCleanupFailed`, whose Display tells the operator:

> health alert baselines may be stale -- run `rm /var/lib/braid/acked-stats.json`
> before trusting `braid monitor`.

That advice is wrong for a probe failure: the acked-stats cleanup
(`alert::drop_ghost_acked_for_devids`) was **never reached**, so nothing is stale and
deleting baselines is pointless. The real situation is that `pool.json` was not saved
(`save_membership` runs only after the per-target loop) and the operation journal is
still pending in `AddPhase::PoolMutation`, so the correct remedy is **`braid recover`**
-- which the message never mentions. Recovery is safe: `recover.rs#execute_add_pool_mutation_recovery`
skips already-live members and re-runs the acked-stats sweep itself.

The same root-cause defect -- the `AckCleanupFailed` message assumes acked-stats
deletion is the remedy while ignoring the unsaved `pool.json` / pending journal --
affects **all four** of its raise-sites, because every one fires before
`save_membership`/`clear_journal`. The two genuine cleanup-failure sites (bootstrap
`remove_acked_stats`, live-pool `drop_ghost_acked_for_devids`) have a milder version:
their message is truthful about acked-stats but omits `braid recover`.

The recover-side twin `RecoverError::AckCleanupFailed` (`cli/src/recover.rs`) already
gets this right ("pending-op.json is preserved ... then re-run `braid recover`"). This
change brings the add-side messages in line with it.

**Intended outcome:** the post-add probe / device-not-found failure renders a message
that names the real state (added but not persisted, journal pending) and directs the
operator to `braid recover`; the surviving `AckCleanupFailed` message also points at
`braid recover`. No control-flow change -- the fail-closed behavior is intentional and
stays.

## Scope decision

Both decisions confirmed with the user:
- **Fix all four sites** (ideal), not just the two probe sites.
- New variant named **`PostAddProbeFailed`** (aligns with the existing `"post-add probe"`
  stage label and the `cmd_add_post_add_probe_uncertainty_is_fatal` test).

Explicitly **not** in scope: changing the fail-closed control flow (the "cannot prove
devid -> stop" behavior is required by the mutation-safety heuristics in CLAUDE.md and
pinned by the existing test's intent); a shared message const between the two
`AckCleanupFailed` variants (not feasible -- `thiserror` `#[error(...)]` needs literal
format strings, and the prefixes/"run" vs "re-run" differ).

## Changes

All changes are in `cli/src/add.rs`. No other source files change: the CLI boundary
(`cli/src/main.rs#main`, ~533-562) renders `AddError` via `e.to_string()` and exits 1
with no per-variant match arm, there are no exit-code/classification tables keyed on
`AddError`, and `result_large_err` is allowed workspace-wide (`Cargo.toml` lints), so a
new String-bearing variant is free.

### 1. Add the `PostAddProbeFailed` variant to `AddError` (enum at `add.rs:44-103`)

Add a new variant with a doc comment (project requires one justifying the boundary,
matching the `DuplicateUuid` / `TargetUuidMismatchAtOpen` style at `add.rs:54-90`):

```rust
/// Post-mutation, pre-persist failure in the live-pool add loop:
/// `btrfs device add` already committed membership, but the follow-up
/// `probe_pool` failed (or did not yet list the new device), so braid
/// stopped before `save_membership` wrote pool.json. Distinct from
/// `AckCleanupFailed` because acked-stats was never reached -- the
/// PoolMutation journal is still pending, so the remediation is
/// `braid recover` (which replays the journal and skips already-live
/// members), not deleting alert baselines.
#[error(
    "disk added to pool, but pool.json was not persisted: {detail}\n\
     pending-op.json is preserved -- run `braid recover` to finish persisting pool membership."
)]
PostAddProbeFailed { detail: String },
```

The variant carries only `detail: String` (the `stage` field is dropped -- the variant
name now encodes the stage). The `detail` strings are preserved verbatim from the
current sites so the disk name and underlying probe error stay in the message.

### 2. Reword the surviving `AckCleanupFailed` message (`add.rs:48-53`)

Mirror `RecoverError::AckCleanupFailed` (`recover.rs:51-56`): keep the acked-stats
remediation (still relevant at the bootstrap + live-pool sites, where the cleanup
genuinely failed) but add the pending-op / recover guidance:

```rust
#[error(
    "pool was modified, but acked-stats cleanup failed at {stage}: {detail}\n\
     pending-op.json is preserved -- rm /var/lib/braid/acked-stats.json before trusting \
     `braid monitor`, then run `braid recover` to finish."
)]
AckCleanupFailed { stage: &'static str, detail: String },
```

This variant remains in use only at the two genuine `alert::*` cleanup-failure sites
(bootstrap `remove_acked_stats` at `add.rs:1421`; live-pool `drop_ghost_acked_for_devids`
at `add.rs:1469`).

### 3. Repoint the two probe sites to the new variant (`add.rs:1456-1468`)

In the live-pool loop, swap the two `AckCleanupFailed { stage: "post-add probe", ... }`
constructions for `PostAddProbeFailed { detail: ... }`, keeping the existing `detail`
expressions:

- probe failure (`~1456`): `format!("{}: {e}", target.name)`
- device-not-found (`~1462`): `format!("{}: not found in pool after add", target.name)`

The third construction in the loop -- `drop_ghost_acked_for_devids` at `~1469`, stage
`"live-pool add"` -- stays `AckCleanupFailed`.

## Tests (`cli/src/add.rs`, `#[cfg(test)]`)

Only one test changes its variant expectation; the reword is invisible to the others
because the project convention pins the typed variant + `stage`, not Display text
(`add.rs:5987-5988`).

- **`cmd_add_post_add_probe_uncertainty_is_fatal`** (`~6107-6158`): change the match arm
  from `Err(AddError::AckCleanupFailed { stage, .. })` / `assert_eq!(stage, "post-add probe")`
  to `Err(AddError::PostAddProbeFailed { .. })`, and update the panic string. Both
  table cases ("probe failure", "mapper omitted") still flow through this arm. Update the
  intent/why/scenario preamble (`~6093-6106`) to state the new variant directs the
  operator to `braid recover` (not acked-stats deletion). Keep the existing
  `added_mappers() == ["braid-disk2"]` assertion.
- **Strengthen the same test** to pin the recover-pointing contract structurally: assert
  `journal::load_journal(&paths).unwrap().is_some()` (the journal survives the error, so
  `braid recover` has something to replay). This is the behavioral promise of the new
  message and is structure-insensitive. (Optionally also assert
  `membership::load_membership(&paths)` does not yet list disk2, confirming pool.json
  was not persisted -- secondary to the journal assertion.)
- **`cmd_add_bootstrap_acked_cleanup_failure_is_fatal`** (`~6051-6086`, asserts
  `stage == "bootstrap"`) and **`cmd_add_live_pool_acked_cleanup_parse_failure_is_fatal`**
  (`~5995-6030`, asserts `stage == "live-pool add"`): **unchanged** -- they keep using
  `AckCleanupFailed` and assert only `stage`, so the message reword does not touch them.

## Docs

No doc changes required. No file under `docs/` quotes the add-time error wording, this
is a message-only change (no behavior/invariant change), and the generic pending-op ->
`braid recover` flow that `braid status`/`doctor` already surface
(`journal::pending_op_advisories`, `journal.rs:273-283`) and that
`docs/guides/recovery-scenarios.md` documents remains accurate.

## Verification

1. `cargo check` (or `cargo build`) -- confirms the new variant compiles and that no
   exhaustive `match` outside `add.rs` breaks (none exists; `main.rs` uses `Display`).
2. `just test-rust` -- runs the CLI unit tests, including the updated
   `cmd_add_post_add_probe_uncertainty_is_fatal` and the unchanged bootstrap/live-pool
   cleanup tests. (Crate package is `braid-cli`; `just test-rust` is the canonical runner.)
3. No VM tests needed. This is a localized Rust error-taxonomy/message change with no
   systemd/module/mount blast radius, so per the CLAUDE.md test-scope guidance a focused
   `just test-rust` suffices; `just test-vm` is not warranted.

Manual reachability note: the probe-failure and device-not-found branches are not easily
triggered against a real pool, but the existing test runners
`AddFullPathRunner::live().with_post_add_probe_failure()` and
`.with_new_mapper_omitted_from_probe()` exercise both branches in-process, so coverage is
complete without a VM.
