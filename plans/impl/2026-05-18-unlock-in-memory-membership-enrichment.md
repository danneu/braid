# Plan: unlock's post-mount enrichment uses the in-memory membership

## Context

`cmd_unlock` already receives `&PoolMembership` via `UnlockParams`
(`cli/src/unlock.rs:25`). After a successful mount, it calls
`membership::refresh_pool_metadata` (`cli/src/unlock.rs:147`), which re-loads
`pool.json` from disk, runs a `MembershipError::Corrupt` -> sidecar branch on
parse error, then calls `enrich_from_pool_state` + `save_membership`. Under
the held pool flock (`modules/braid/braid-wrapper.sh:52-61`), the on-disk
file and the membership already in `params.membership` are identical, so the
disk reload + corruption-sidecar arms are dead paths in this context. Unlock
also discards the `RefreshOutcome` -- the structured `EnrichmentReport` that
motivated `refresh_pool_metadata`'s return shape is computed and thrown away.

`refresh_pool_metadata` has exactly one non-test caller today: `unlock.rs:147`.
Its `RefreshOutcome::Refreshed { report }` shape exists for `braid doctor`'s
planned Phase 5 `foreign-luks-uuid` wiring
(`plans/impl/2026-05-12-luks-uuid-as-identity/plan.md:1331-1367`). The helper
stays untouched for that future caller; only unlock pivots away from it.

The pattern the pivot adopts already exists for `add.rs:1318-1320` and
`replace.rs:854-873`: clone the in-memory membership, call
`enrich_from_pool_state`, then `save_membership`. Unlock joins them and stops
introducing a third API shape.

## Change

Replace the `if let Ok / Err` block at `cli/src/unlock.rs:145-152` with a
direct enrich + save against the in-memory membership. Probe-failure stays a
warning via `emit_status`; enrich-failure and save-failure are warnings via
`eprintln!` (byte-identical to the lines previously emitted from inside
`refresh_pool_metadata` at `cli/src/membership.rs:658,663`). Best-effort
semantics are preserved: the whole block is a fall-through warning chain --
nothing in it can fail `cmd_unlock`.

New shape:

```rust
match probe::probe_pool(runner, fs, mount_point) {
    Ok(pool_after) => {
        let mut enriched = params.membership.clone();
        match membership::enrich_from_pool_state(&mut enriched, &pool_after) {
            Ok(_report) => {
                if let Err(e) = membership::save_membership(&enriched, params.paths) {
                    eprintln!("Warning: failed to save enriched membership: {e}");
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to enrich pool membership: {e}");
            }
        }
    }
    Err(e) => crate::status_tag::emit_status(&format!(
        "Warning: failed to probe pool for metadata refresh: {e}\n"
    )),
}
```

Rewrite the preceding comment block (`cli/src/unlock.rs:132-144`) to describe
the new shape. Drop the `refresh_pool_metadata` references; preserve the
"best-effort, correctness never depends on this" framing and the two test
pins (`unlock_tolerates_post_mount_probe_mounted_false`,
`unlock_warns_when_post_mount_probe_errors`). Add a one-line note that the
in-memory clone is authoritative because the wrapper holds the pool flock for
the lifetime of `unlock`.

Reword the `UnlockPlan::execute` contract block at `cli/src/unlock.rs:85-92`.
Today bullet 2 says "Membership comes from pool.json; unlock never creates,
repairs, or rewrites it", which contradicts bullet 5's "After a successful
mount, pool.json enrichment fields (devid, added_at) are refreshed
best-effort". The pivot makes that contradiction sharper by replacing the
indirection through `refresh_pool_metadata` with a direct `save_membership`
call. Narrow bullet 2 to the invariant it actually expresses -- unlock
doesn't touch membership topology -- and leave bullet 5 as the concrete
elaboration of what runtime metadata may change. Example:

```rust
// Contract:
// - Pure operator command: bring the pool online from authoritative state.
// - Membership comes from pool.json; unlock never mutates membership
//   topology and never creates or repairs invalid/missing membership.
// - Probe only configured members, open what is available, and mount the pool.
// - Refuse degraded mounts unless --allow-degraded is explicit.
// - After a successful mount, pool.json enrichment fields (devid,
//   added_at) are refreshed best-effort, but correctness never
//   depends on that write.
```

The exact wording is the implementer's call; the requirement is that bullet 2
names "topology" (or equivalent) rather than "rewrites", so it is consistent
with bullet 5.

Reword the `plan_unlock` skip-rationale comment at `cli/src/unlock.rs:175-177`.
Today it says "unlock never writes pool.json membership", which becomes
actively misleading after this pivot because the execute path explicitly
writes runtime metadata. The skip-rationale for
`check_pool_unlocked_if_membership_exists` rests on the narrower invariant
that unlock never mutates membership topology (the set of disks). Restate it
along those lines, e.g.:

```rust
// `plan_add` also runs `check_pool_unlocked_if_membership_exists`
// here; unlock skips it because unlock never mutates pool.json
// membership topology -- execute may refresh runtime metadata
// (devid, added_at) after mount, but the set of members is
// authoritative on entry. See the "Contract:" block in
// `UnlockPlan::execute`.
```

The exact wording is the implementer's call; the requirement is that the
comment names "topology" (or equivalent) rather than "writes", so the next
reader understands which invariant gates the skip.

### Why warning-and-continue, not `?`

`add.rs` and `replace.rs` propagate `enrich_from_pool_state` errors with `?`
because their membership write is part of the atomic mutating op; failing
the command on a write hiccup is acceptable (the journal still covers
recovery). Unlock's mount has already succeeded by the time the enrichment
runs and the documented contract is "correctness never depends on this
enrichment" (`unlock.rs:142`). Hard-failing the whole `cmd_unlock` after a
green mount because pool.json couldn't be re-serialized would be a
regression. The explicit `match` arm also future-proofs the call site if
`enrich_from_pool_state` ever grows a real `Err` path -- today its body has
no `Err` returns.

## Files modified

- `cli/src/unlock.rs` -- four edits:
  1. Replace `cli/src/unlock.rs:132-152` (the post-mount enrichment comment
     and the `if let Ok / Err` block) with the new shape.
  2. Reword the `UnlockPlan::execute` contract block at
     `cli/src/unlock.rs:85-92` so bullet 2 names "membership topology"
     instead of "rewrites it", resolving its contradiction with bullet 5.
  3. Reword the skip-rationale comment at `cli/src/unlock.rs:175-177` to
     name "membership topology" instead of "writes pool.json membership".
  4. Add one new unit test below the existing post-mount probe tests --
     see "Test impact" for shape.

That is the entire diff. No other file changes.

## What stays untouched

- `membership::refresh_pool_metadata` and `RefreshOutcome`
  (`cli/src/membership.rs:562-666`) -- reserved for doctor's Phase 5 wiring
  per `plans/impl/2026-05-12-luks-uuid-as-identity/plan.md:1357-1361`.
- `membership::write_corrupt_sidecar` and `CorruptSidecarError`
  (`cli/src/membership.rs:668-`) -- only used by `refresh_pool_metadata`.
- The corruption-sidecar test at `cli/src/membership.rs:1351-1397` --
  exercises `refresh_pool_metadata` directly, unaffected by the unlock pivot.
- `add.rs` and `replace.rs` enrichment call sites -- the precedent we are
  joining, no changes.
- All warning wording -- the three new strings are byte-identical to the
  ones the test at `cli/src/unlock.rs:988-1053`
  (`unlock_warns_when_post_mount_probe_errors`) and the discarded paths inside
  `refresh_pool_metadata` emit today.

## Test impact

Existing tests carry over unchanged; one new unit test is added to pin the
save-failure best-effort contract.

Carry-over (no changes):

- `unlock_tolerates_post_mount_probe_mounted_false`
  (`cli/src/unlock.rs:1065-1166`) -- pins that a `mounted=false` post-mount
  probe leaves `devid`/`added_at` unchanged. After the pivot, the
  `Ok(PoolState { devices: vec![] })` branch still enters
  `enrich_from_pool_state`, walks zero devices, no fields change, and
  `save_membership` writes back the same bytes. Behavior preserved.
- `unlock_tolerates_post_mount_probe_err` (`cli/src/unlock.rs:1179-1283`) --
  pins that a probe `Err` leaves pool.json untouched. After the pivot, the
  `Err(e)` arm only emits the existing warning and skips the
  enrich+save block. Behavior preserved.
- `unlock_warns_when_post_mount_probe_errors` (`cli/src/unlock.rs:988-1053`)
  -- pins exact "Warning: failed to probe pool for metadata refresh: " text.
  The line moves from line 149-151 of unlock.rs to the same call but the
  string is byte-identical. Test passes unchanged.
- `refresh_pool_metadata_corrupt_writes_sidecar_and_leaves_original`
  (`cli/src/membership.rs:1351-1397`) -- pins corruption-sidecar behavior
  inside `refresh_pool_metadata`, which is unmodified. Test passes unchanged.
- VM test `Test 2b: unlock enriches pool.json with runtime metadata`
  (`tests/cli/braid-unlock.py:301-322`) -- end-to-end asserts `devid` is
  populated in `pool.json` after `braid unlock`. The happy path
  (`Ok(pool_after)` + successful enrich + successful save) writes the same
  bytes as today. Test passes unchanged.

New test: `unlock_tolerates_post_mount_save_membership_failure`

- **Intent:** `cmd_unlock` must tolerate a `save_membership` failure on the
  post-mount enrichment path without failing the command.
- **Why it exists:** Regression-pin against an implementer accidentally using
  `?` on `save_membership` after the pivot. The mount has already succeeded
  by the time enrichment runs; turning a pool.json write hiccup into a hard
  `cmd_unlock` failure would silently break the wrapper's
  post-success path (it would treat the pool as not mounted when it actually
  is). The two existing `unlock_tolerates_post_mount_probe_*` tests cover the
  probe-failure and probe-`mounted=false` arms; this third test closes the
  third arm.
- **Scenario:** 3-disk pool, clean mount, post-mount `probe_pool` returns
  `Ok(...)` with the expected devices, but the eventual
  `save_membership` call fails. Force the failure with the same mechanism
  the existing `save_membership_failure_classified_as_membership_persist`
  test uses (`cli/src/remove.rs:1495-1515`): seed nothing at the pool.json
  path; instead, before invoking `cmd_unlock`, place a regular file where a
  required directory component for `paths.pool_json()` would live (or, if
  `paths.pool_json()`'s parent already exists in the `isolated_paths()`
  fixture, replace `paths.pool_json()` itself with a directory) so the
  atomic write inside `save_membership_to` errors.
- **Assertions:**
  1. `cmd_unlock(...).is_ok()` -- the command returns `Ok(())` despite the
     save failure.
  2. The pool is left mounted (`mount::execute_unlock_and_mount` was reached
     and succeeded; verify via the runner's request log, mirroring the
     pattern in the two existing post-mount tolerance tests).
  3. Stderr contains exactly one occurrence of the literal
     `"Warning: failed to save enriched membership: "` prefix, mirroring the
     assertion style in `unlock_warns_when_post_mount_probe_errors`
     (`cli/src/unlock.rs:988-1053`).

The enrich-failure arm stays untested. `enrich_from_pool_state` has no
`Err` returns in its body today (`cli/src/membership.rs:594-621`), so the
`Err(e) => eprintln!("Warning: failed to enrich pool membership: {e}")` arm
is unreachable. We keep it for parity with the warning shape
`refresh_pool_metadata` had at `cli/src/membership.rs:658`, in case
`enrich_from_pool_state` ever grows a real `Err` path. Testing an
unreachable arm would violate the "behavioral and structure-insensitive"
bar.

## Verification

Run from the repo root.

```
just test-rust
```

This runs the unit tests, including the four post-mount unlock tolerance
tests (the three carry-overs plus the new
`unlock_tolerates_post_mount_save_membership_failure`) and the membership
corruption-sidecar test.

```
just test-vm braid-unlock
```

Runs the VM test that asserts end-to-end enrichment.

Manual smoke (optional, against a live test VM):

```
sudo braid unlock
sudo jq '.members[].devid' /var/lib/braid/pool.json
```

Every `devid` should be non-null after the command, matching today's
behavior.

## Non-goals

- Do not delete `refresh_pool_metadata`, `RefreshOutcome`,
  `write_corrupt_sidecar`, or `CorruptSidecarError`. They are reserved for
  doctor's Phase 5 wiring.
- Do not factor a shared `enrich_in_memory_and_save` helper across add,
  replace, and unlock. The surrounding logic differs (add has bootstrap and
  live-pool branches; replace stamps `added_at` post-enrich; unlock uses
  warning-and-continue), so a shared helper would push divergence into the
  call sites without simplifying them. Three sites, three direct uses of
  `enrich_from_pool_state` + `save_membership`.
- Do not change warning wording. The three strings are byte-identical to
  what the code already emits today (one from `unlock.rs`, two from inside
  `refresh_pool_metadata`).
- Do not add additional unit tests beyond the one
  `unlock_tolerates_post_mount_save_membership_failure` described above.
  Carry-over tests already pin the probe-failure and probe-`mounted=false`
  arms; the enrich-failure arm is unreachable today.
