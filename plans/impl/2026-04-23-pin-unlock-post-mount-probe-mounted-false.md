# verify-this-finding-and-moonlit-badger

## Context

A code review flagged a "Low" finding about `cmd_unlock`'s best-effort
`probe_pool` guard at `cli/src/unlock.rs:93-96`:

> Low -- `cmd_unlock` best-effort `probe_pool` guard: enrichment is
> "skipped silently" when `probe_pool` returns `mounted=false`.

The user's clarification: the claim is mostly right but slightly inaccurate.
`refresh_pool_metadata` does run (the `Ok(_)` arm is entered), but its inner
loop over `pool.devices` is empty when `mounted=false`, so the effect is
equivalent to a no-op enrichment. The review recommends a comment-only fix.

A plan-review pass then raised a valid Medium finding: the "no behavior
change warranted" claim is untested. The existing test
`unlock_warns_on_paused_balance` never mocks `FindmntJson` for the
post-mount probe, so its `probe_pool` call returns `MissingMock` -- it
exercises the `Err` arm of the `if let Ok(...)` guard, not the
`Ok(mounted: false)` arm described by the finding. Nothing in
`cli/src/unlock.rs` or `tests/cli/braid-unlock.py` would fail if
`cmd_unlock` stopped tolerating the `mounted: false` post-mount probe.

**Pivot:** add the regression test first, confirm it pins the no-op
behavior, *then* tighten the comment.

## Verification of the finding

- `cli/src/unlock.rs:94` wraps the enrichment in
  `if let Ok(pool_after) = probe::probe_pool(runner, mount_point)`. Any
  `Ok` -- including `mounted=false` -- enters the arm.
- `cli/src/probe.rs:229-242`: when `findmnt` shows no exact match for the
  mount point, `probe_pool` returns
  `Ok(PoolState { mounted: false, devices: vec![], ... })`. No error.
- `cli/src/membership.rs:179-202` (`refresh_pool_metadata`): loads
  membership, iterates `&pool.devices` (empty when `mounted=false`, so no
  field updates), then re-saves membership. State-preserving but performs
  unnecessary load+save I/O.
- `cli/src/add.rs:605` and `cli/src/replace.rs:380` use the same pattern
  (inlined enrichment loop). Consistent across all three post-commit
  enrichment sites.
- Contract at `cli/src/unlock.rs:57-63` already says correctness never
  depends on the enrichment write.

So the finding's core claim is correct; the only inaccuracy is *how* it
is skipped (empty-loop no-op, not an `if`-branch skip). No behavior
change is warranted -- but we need a pinning test before we can say that.

## Plan

### Step 1: add a pinning unit test in `cli/src/unlock.rs`

New test: `unlock_tolerates_post_mount_probe_mounted_false`.

- Block-comment header per project test conventions: Intent / Why /
  Scenario.
- Pre-populate `pool.json` at `sp` with `disk1`, `disk2`, `disk3` whose
  `luks_uuid` and `devid` are `None`, via `save_membership(&m, &sp)`.
  This gives the test a concrete assertion: those fields must remain
  `None` after `cmd_unlock` returns.
- Mock the happy-path unlock commands (mirror
  `unlock_warns_on_paused_balance` lines 628-711): mountpoint check,
  3x `CryptsetupLuksUuid`, `CryptsetupTestPassphrase`,
  3x `CryptsetupLuksOpen`, `BtrfsDeviceScanAll`, `Mount`.
- **Key difference from the paused-balance test:** mock the post-mount
  `FindmntJson` call with exit_status=1 and empty stderr. Per
  `cli/src/parse/findmnt.rs:30-42` that parses to
  `FindmntOutput { filesystems: vec![] }`, so `probe_pool` returns
  `Ok(PoolState { mounted: false, devices: vec![], ... })` at
  `cli/src/probe.rs:229-242`.
- Mock `BtrfsBalanceStatus` with the "No balance found" stdout so the
  trailing `emit_paused_balance_warning` call succeeds.
- Include the usual `with_luks_dump_text_luks2_for(&[...])` stubs.
- Assertions:
  1. `cmd_unlock(...)` returns `Ok(())`.
  2. `load_membership(&sp)` round-trips; `disk1`/`disk2`/`disk3` each
     still have `luks_uuid == None` and `devid == None`. This pins
     "no enrichment happened when `mounted=false`."

### Step 2: confirm the pinning test passes on unchanged code

- `cargo test -p braid unlock_tolerates_post_mount_probe_mounted_false`
  must pass with the comment still in its original form.
- Then mutation-check it locally: temporarily change
  `if let Ok(pool_after) = ...` to `let pool_after = ...?;` and verify
  the test would fail if the tolerance went away. (This is a one-off
  local check; no commit.)

### Step 3: tighten the comment at `cli/src/unlock.rs:93`

Only after step 2 confirms the test pins the behavior, replace:

```rust
// Enrich pool.json with live metadata (luks_uuid, devid) — best-effort.
if let Ok(pool_after) = probe::probe_pool(runner, mount_point) {
    membership::refresh_pool_metadata(&pool_after, params.paths);
}
```

with:

```rust
// Enrich pool.json with live metadata (luks_uuid, devid) -- best-effort.
// A rare race where probe_pool sees mounted=false after a successful
// mount leaves `pool_after.devices` empty, so refresh_pool_metadata
// no-ops. That is acceptable: correctness never depends on this write
// (see contract above). Pinned by
// unlock_tolerates_post_mount_probe_mounted_false.
if let Ok(pool_after) = probe::probe_pool(runner, mount_point) {
    membership::refresh_pool_metadata(&pool_after, params.paths);
}
```

Changes in this comment:
- `—` -> `--` (project ASCII style for code comments).
- Names the `mounted=false` no-op case explicitly.
- Cites the pinning test by name so future readers can jump to the
  regression anchor.

### Critical files

- `cli/src/unlock.rs` -- new test + comment tightening.

### Out of scope

- No change to the `if let Ok(...)` control flow.
- No change to `refresh_pool_metadata` (the load+save on an empty
  device list is a minor I/O quirk; `add.rs` and `replace.rs` share
  the same shape, so any restructuring should sweep all three
  callers and is beyond this finding).
- Other em-dashes in `unlock.rs` (lines 87, 425, 471, 608) are not
  touched; bundling a file-wide ASCII sweep would widen the blast
  radius of a comment tightening.

## Verification steps

1. `cargo fmt` -- no formatting regressions.
2. `cargo test -p braid unlock_tolerates_post_mount_probe_mounted_false`
   -- new test passes.
3. `just test-rust` -- full rust suite still green after comment edit.
4. `grep -n 'unlock_tolerates_post_mount_probe_mounted_false'
   cli/src/unlock.rs` -- confirms comment references the test by its
   actual name.
