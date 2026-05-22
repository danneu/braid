# Delete `replay_post_mutation`; route Add callers through `replay_owed_raid1_maintenance`

## Context

`cli/src/recover.rs` exposes a helper `replay_post_mutation` at
`cli/src/recover.rs:1833-1913` that matches on `journal::OpKind` with
three arms:

- `Add { .. }` -- resume any paused balance, then replay the soft RAID1
  maintenance balance.
- `Remove { .. }` -- no-op, with a long justification comment.
- `RemoveMissing { .. } | Replace { .. }` -- returns
  `Err("internal error: phased replace/remove-missing recovery reached
  generic replay")`.

Three problems compound:

1. The `RemoveMissing | Replace` arm is **unreachable**. Dispatch at
   `cli/src/recover.rs:1413-1486` routes those ops to dedicated
   executors; only `Add` and `Remove` land in
   `RecoverCompletion::GenericLivePool` and reach this helper. Commit
   `e14ee8f` made the upstream match exhaustive but left this
   defensive arm behind as dead text.
2. The Add body is a **duplicate of an existing helper**:
   `replay_owed_raid1_maintenance` at `cli/src/recover.rs:1753-1805`
   already does paused-balance resume + soft RAID1 replay, parameterized
   by a `label: &str`. Other recovery executors already call it with
   `"remove-missing"` (`:2846-2852`) and `"replace"` (`:3323-3329`).
3. The runtime gate **re-derives a decision the typed plan already
   carries**. `RecoverCompletion::GenericLivePool` stores
   `replay_raid1_maintenance: bool` (`:265-267`); it is `true` for Add,
   `false` for Remove (set at `:1480-1485`). The preview consumes it at
   `:541`. The runtime executor ignores it and matches on
   `journal.op` inside `replay_post_mutation` instead -- a second
   semantic gate, contrary to the typed-plan model in
   `docs/decisions/022-dry-run-preview-model.md`.

This change deletes `replay_post_mutation` outright (no replacement
helper) and routes both Add call sites through the existing
`replay_owed_raid1_maintenance`, gated by the `replay_raid1_maintenance`
boolean already on the plan. Preview and execution share one semantic
decision. No behavior change.

## Plan

All edits in `cli/src/recover.rs`.

### 1. Delete `replay_post_mutation` and its doc comment

Remove `cli/src/recover.rs:1807-1913` (the multi-paragraph doc comment
at `:1807-1832` and the function itself at `:1833-1913`). The
"`OpKind::Remove` is intentionally skipped" rationale moves to the
construction site (step 4 below). `journal_op_label` keeps its other
caller (`format_recover_entry` at `:1151`) and stays.

### 2. Rewire `execute_add_post_balance_recovery` (`:2337-2343`)

Replace the `replay_post_mutation(..., &journal.op, ...)` call with a
direct call to the existing helper:

```rust
replay_owed_raid1_maintenance(
    runner,
    params.config.mount_point(),
    "add",
    &pool,
    params.progress,
)?;
```

This function is Add-by-construction (reached only from `AddPostBalance`
completion and from `execute_add_pool_mutation_recovery` at `:2649` and
`:2670`), so no gate is needed.

### 3. Thread `replay_raid1_maintenance` into the generic executor

Three call-graph touches:

- Dispatch at `:691-693`: destructure the variant and pass the boolean:
  ```rust
  RecoverCompletion::GenericLivePool { replay_raid1_maintenance } => {
      execute_generic_live_pool_recovery(
          runner, by_id_resolver, params, plan, pool,
          *replay_raid1_maintenance,
      )
  }
  ```
- `execute_generic_live_pool_recovery` signature at `:1028-1034`: add
  `replay_raid1_maintenance: bool` as a parameter.

In the function body, replace the unconditional
`replay_post_mutation(... &plan.journal.op ...)` at `:1120-1126` with a
boolean gate using the existing helper:

```rust
if replay_raid1_maintenance {
    replay_owed_raid1_maintenance(
        runner,
        &plan.mount_point,
        "add",
        &pool,
        params.progress,
    )?;
}
```

The pre-existing `if matches!(... Add ...) && pre_membership.is_empty()`
guard at `:1111-1118` (bootstrap acked-stats cleanup) is unrelated and
stays as-is.

### 3a. Thread the boolean through direct test call sites

`execute_generic_live_pool_recovery` has six direct test call sites
that bypass `cmd_recover` / `RecoverPlan::execute`. The signature
change in step 3 turns each into a compile error until the new
argument is supplied. Thread `true` for Add (bootstrap) cases and
`false` for Remove cases:

- `:5665` -- `bootstrap_recovery_clears_acked_stats` (bootstrap Add) -> `true`.
- `:6230` -- `remove_recovery_drops_target_devid_when_eviction_committed` -> `false`.
- `:6267` -- `remove_recovery_preserves_target_devid_when_eviction_uncommitted` -> `false`.
- `:6303` -- `remove_recovery_with_no_devid_journal_skips_cleanup_with_warning` -> `false`.
- `:6336` -- `remove_recovery_warning_only_on_corrupt_acked_stats` -> `false`.
- `:6466` -- `bootstrap_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal` (bootstrap Add) -> `true`.

### 4. Move the "why Remove is `false`" rationale to the source of truth

The 13-line justification currently inside the deleted Remove arm
(`:1891-1903`) becomes a `//` comment block immediately above the
`replay_raid1_maintenance: false` assignment at `:1483-1485`. The
field-doc-comment-style attachment makes it visible at both the
preview and runtime call sites (since both read from this variant).

Verbatim content to carry over (paraphrased here, keep wording in the
move):

- `braid remove` is the only mutation whose pre-mutation phase issues a
  balance (the RAID1 -> single conversion in the 2->1 case).
- Resuming a paused balance during Remove recovery would complete the
  conversion to single without removing the device, then journal clear
  would silently halve redundancy.
- The recovery_guidance message directs the operator to re-run
  `braid remove`, which handles every shape (2->1 pre-balance, 3->2 /
  4->3 with no pre-balance) correctly.

### 5. Comment / reference sweep

Update remaining `replay_post_mutation` references in nearby code
comments. The replacement name varies by context: most point at the
work `replay_owed_raid1_maintenance` now does; two are already stale
and misattribute Replace work.

Rename-only (comments are accurate, just use the deleted name):
`:11654-11655`, `:11804-11805`, `:12680`, `:13486`, `:14619`,
`:14678-14680`. Replace `replay_post_mutation` with
`replay_owed_raid1_maintenance` in each.

Rename **and** correct (comments mislabel which helper runs the work):
- `:13983-13985` -- in a Replace test fixture, claims
  `replay_post_mutation` resolves the new device's devid. Replace and
  RemoveMissing run their resize through
  `execute_replace_post_maintenance_recovery` /
  `execute_remove_missing_post_maintenance_recovery`, not the helper
  being deleted. Re-attribute to the actual post-maintenance executor.
- `:14381-14384` -- section header `// ── replay_post_mutation ──`
  immediately above a `BtrfsFilesystemResize` mock in a Replace test.
  Replace-side resize lives in the Replace post-maintenance executor;
  retitle accordingly.

## Tests

The existing pin coverage is asymmetric: the Remove false-branch is
well-pinned end-to-end, but the Add true-branch through
`RecoverCompletion::GenericLivePool` -> `RecoverPlan::execute` ->
`execute_generic_live_pool_recovery` is not pinned through the typed
plan -- only through a direct executor call that takes the boolean as
input. Add one focused test that enters at `cmd_recover`, the same
boundary the Remove test uses, so it covers the construction step at
`:1480-1485` as well as the runtime gate.

**Already pins the Remove false-branch end-to-end** (no change needed):
`recover_skips_paused_balance_resume_for_remove` at
`cli/src/recover.rs:14633-14705` enters via `cmd_recover` (`:14685`)
and deliberately does **not** mock `BtrfsBalanceStatus`,
`BtrfsBalanceResume`, or `BtrfsBalanceRaid1Soft`. If `plan_recover`
ever flips Remove to `replay_raid1_maintenance: true`, or the runtime
gate inverts, recover hits one of the unmocked balance commands and
the test fails with `MissingMock`. Holds equally well after the pivot.

**New test for the Add true-branch end-to-end**: add a unit test that
mirrors the Remove no-op test's entry pattern but for the bootstrap
Add path. Enter via `cmd_recover` so `plan_recover` constructs
`RecoverCompletion::GenericLivePool { replay_raid1_maintenance: true }`
and `RecoverPlan::execute` threads it into the executor -- this is
what fails if the value at `:1480-1485` regresses, not just the helper
call. Sketch:

```rust
// Intent
// cmd_recover for a bootstrap-Add journal issues the post-mutation soft
// RAID1 balance.
//
// Why it exists
// Pivot moved the runtime decision out of replay_post_mutation's
// OpKind match and into `RecoverCompletion::GenericLivePool.
// replay_raid1_maintenance`, set at plan-construction time. If that
// value silently flips to false for Add, or the executor stops
// consuming it, recovery would clear the journal without replaying
// the soft RAID1 balance and leave the operator with single-profile
// chunks. This test fails the moment either end of the contract
// regresses.
//
// Scenario
// A 2-disk bootstrap-Add (`bootstrap_pool_mutation_add_journal`)
// crashed after btrfs created the filesystem; recovery enters with
// the live pool already showing both disks, replays the owed
// maintenance, and clears the journal.
#[test]
fn cmd_recover_bootstrap_add_replays_owed_raid1_maintenance() {
    let f = PoolFixture::empty();
    let fs = MockFs::new(&[]);
    let journal = bootstrap_pool_mutation_add_journal();
    journal::write_journal(&f.paths, &journal).unwrap();

    // mountpoint + probe plumbing for cmd_recover -- mirror the
    // shape used by recover_skips_paused_balance_resume_for_remove
    // (mountpoint_ok, BtrfsFilesystemShow, per-mapper CryptsetupStatus
    // + LuksUuid), plus `with_balance_replay` so the post-mutation
    // soft RAID1 mock is registered.
    let (mp_req, mp_out) = mountpoint_ok();
    let runner = with_balance_replay(
        MockRunner::default()
            .with_output(mp_req, mp_out)
            // ... two-disk show + cryptsetup pinning per the Remove
            // test pattern at :14647-14675 ...
    );

    let resolver = resolver_for(&[
        ("/dev/vda", "virtio-disk1"),
        ("/dev/vdb", "virtio-disk2"),
    ]);
    cmd_recover(
        &runner,
        &fs,
        &resolver,
        &f.recover_params().passphrase_file(None).build(),
    )
    .expect("bootstrap-Add recovery should replay maintenance");

    let requests = runner.requests();
    assert!(
        requests.iter().any(|r| matches!(
            r,
            CmdRequest::BtrfsBalanceRaid1Soft { mount_point }
                if mount_point.as_str() == "/mnt/storage"
        )),
        "cmd_recover Add path must issue post-mutation soft RAID1 balance"
    );
    assert!(
        !f.paths.pending_op_json().exists(),
        "journal must clear after successful maintenance replay"
    );
}
```

Citations supporting the choice of `cmd_recover` over a direct
executor call:

- `recover_skips_paused_balance_resume_for_remove` (`:14633`) shows the
  pattern: full `cmd_recover` entry, MissingMock-driven contract
  assertion. Mirroring it is what makes preview/runtime stay coupled.
- The existing direct-call test `bootstrap_recovery_clears_acked_stats`
  (`:5647-5678`) -- which the step 3a sweep will update to pass `true`
  -- only asserts `acked-stats.json` removal and never checks that
  `BtrfsBalanceRaid1Soft` was hit, so a regression in the boolean
  default for Add would leave it green even after step 3a.
- The dry-run preview-only test
  `render_add_recovery_existing_luks_with_enroll_renders_addkey_before_scanforget`
  (`:8050`) exercises step rendering, not runtime command issuance.
  It cannot catch a divergence between the preview gate and the
  runtime gate -- which is exactly the bug class the pivot prevents.

## Verification

1. `just test-rust` -- runs the new positive Add test, the existing
   Remove no-op pin, and the rest of the recover.rs unit tests. This is
   the load-bearing verification.
2. Implicit `cargo check` via `just test-rust` -- a stray reference to
   the deleted `replay_post_mutation` or a missed signature update on
   `execute_generic_live_pool_recovery` will surface at compile time.
3. No VM tests required. The change is structural inside Rust recovery
   code; no systemd, mount, btrfs, or LUKS behavior changes. `just
   test-vm` adds nothing.

## Implementation notes

- Section 5's sweep also touched `tests/cli/braid-recover.py:311` (the
  only remaining `replay_post_mutation` reference outside `cli/src/`).
  Plan said edits were confined to `cli/src/recover.rs`; renamed the
  stray reference in the Python harness for the same reason as the Rust
  comments -- the symbol no longer exists.
- The Remove test's MissingMock note (`recover.rs:15131`) was lightly
  reworded rather than just renamed: the original wording ("If
  replay_post_mutation regresses and either probes balance status...")
  no longer parses now that the function is deleted, so it now reads
  "If the runtime gate for OpKind::Remove regresses and recover calls
  replay_owed_raid1_maintenance...". Intent and assertion are unchanged.
