# Plan: fix `remove-missing`/`replace --missing-id` null-underlying diagnostic

## Context

Commit `242b460 fix(status): report alert missing devid union` made `braid
status` publish the union of btrfs-authoritative MISSING devids and
null-underlying devids (LUKS mapper open, `(null)` backing) under both
`missing_count` and `missing_devids`. That correctly surfaces hot-unplugged
drives to alerting, but it created a contradiction: when an operator follows
the manual ("look at `missing_devids` in `braid status`, then run
`braid remove-missing --missing-id <N>`"), the CLI rejects null-underlying
devids with `"devid N is not a device in this pool"` -- text that flatly
contradicts what `braid status` just showed. The same trap exists for
`braid replace --missing-id` and, less obviously, for `braid replace
--old <name>` auto-resolution when the persisted devid is currently
null-underlying. The commit papered the issue over with workaround
paragraphs in `manual/commands/remove-missing.md` and
`manual/commands/replace.md` that explicitly say "if you see this error,
that error itself is the signal" -- shifting interpretation work onto the
operator and violating the project's `doctor`/`status`/`unlock`
recovery-hint invariant (`docs/luks-unlock.md`, AGENTS.md "CLI Output
Style") that diagnostic text must match braid's own view.

This change moves the explanation into the errors themselves: each
validation chain now treats `pool.missing_devids` as authoritative (so the
overlap case documented at `cli/src/types.rs:378-383` -- btrfs has
promoted a hot-unplugged devid to MISSING while its mapper still reports
`(null)` -- still proceeds), and emits a specific null-underlying
diagnostic only when the devid is present in `null_underlying` but absent
from `missing_devids`. The manual workarounds get pruned and
`manual/guides/recovery-scenarios.md` gains the subsection the new errors
point at. While in `replace.rs`, `resolve_replace_source` gets unified on
`pool` as its single source of truth (dropping a redundant second probe)
so all three refusal sites read from the same snapshot that produced the
alert union in `braid status`.

## Approach

Authority order, applied everywhere:

1. `pool.devices` (live) -- live-disk refusal.
2. `pool.missing_devids` (btrfs-authoritative MISSING) -- proceed.
3. `pool.null_underlying` (mapper open, backing gone, btrfs has not yet
   promoted) -- hot-unplug refusal with specific wording.
4. Anywhere else -- generic "not a device in this pool".

Three coordinated edits:

1. **`remove_missing.rs`**: split the existing
   `!pool.missing_devids.contains(...)` branch into "null_underlying-only"
   vs "nowhere".
2. **`replace.rs::resolve_replace_source`**: drop the second
   `probe_missing_devids` probe, read from `pool.missing_devids` /
   `pool.null_underlying` directly, and add the null-underlying refusal
   to both the `--missing-id`-supplied and the auto-resolve sub-branches.
   Introduce a small inner helper so the three sites share one wording.
3. **Manual**: prune the two workaround paragraphs and add a
   `Hot-unplug while pool is mounted` subsection to
   `recovery-scenarios.md` using `braid lock` (not raw `umount`) for the
   remount step.

CLI errors stay URL-free and reference commands by name (AGENTS.md "CLI
Output Style"). The manual subsection uses a markdown anchor link
(`#hot-unplug-while-pool-is-mounted`) only from inside the manual itself.

## Code changes

### 1. `cli/src/remove_missing.rs`

**Extract a pure validation helper.** `plan_remove_missing` obtains its
`PoolState` only via `probe_pool` (line 348), and the parser at
`cli/src/parse/btrfs_filesystem_show.rs:107-140` excludes any row with
a `MISSING` marker from `show.devices` into `show.missing_devids`. So
through the existing probe path, `pool.missing_devids` and
`pool.null_underlying` are de-facto disjoint -- the overlap that
`cli/src/types.rs:378-383` documents is a defensive state the
`alert_missing_devids` `BTreeSet` dedup guards against, not one a mock
runner can produce. To cover that overlap behaviorally, pull the
per-target classification out of `plan_remove_missing` into a private
pure helper that takes `&PoolState` and `missing_id`. The integration
test path remains; the helper makes the overlap case unit-testable
with a hand-constructed `PoolState`.

Add a private function near the existing live/missing-id checks:

```rust
/// Classify a `--missing-id N` target against the planning-time pool
/// snapshot. Authority order:
/// 1. `pool.devices` (live) -- refuse with "use braid remove".
/// 2. `pool.missing_devids` (btrfs-authoritative MISSING) -- proceed.
/// 3. `pool.null_underlying` (mapper open, backing gone, btrfs has
///    not yet promoted) -- refuse with hot-unplug diagnostic.
/// 4. Anywhere else -- refuse with generic not-in-pool wording.
///
/// Caller is responsible for the pool-level `missing_count == 0`
/// pre-check; this helper only classifies the supplied devid against
/// an existing snapshot.
fn validate_missing_id_target(pool: &PoolState, missing_id: u64) -> Result<(), String> {
    if pool.devices.iter().any(|d| d.devid == missing_id) {
        return Err(format!(
            "devid {missing_id} is a live device, not a missing one. \
             Use 'braid remove' to remove live devices."
        ));
    }
    if pool.missing_devids.contains(&missing_id) {
        return Ok(());
    }
    if pool.null_underlying.iter().any(|d| d.devid == missing_id) {
        return Err(format!(
            "devid {missing_id} is hot-unplugged but btrfs has not yet \
             promoted it to MISSING (LUKS mapper open, backing device \
             gone). `braid remove-missing` only operates on \
             btrfs-authoritative MISSING devids. Confirm the disk is \
             truly gone, then relock and re-unlock the pool degraded \
             (`braid lock` then `braid unlock --allow-degraded`) so \
             btrfs promotes devid {missing_id}, and retry."
        ));
    }
    Err(format!(
        "devid {missing_id} is not a device in this pool. \
         Use 'braid status' to see device IDs."
    ))
}
```

**Call site:** in `plan_remove_missing`, replace the existing
live-device block (lines 380-388) and the missing-set block
(lines 390-398) with a single call:

```rust
if let Err(msg) = validate_missing_id_target(&pool, params.missing_id) {
    return Err(PlanFailure::with_notes(
        notes,
        RemoveMissingError::Validation(msg),
    ));
}
```

The pool-level `missing_count == 0` pre-check at lines 370-378 stays
ahead of this call, unchanged.

**Test changes in `cli/src/remove_missing.rs`:**

The helper enables direct unit coverage; existing integration tests
through `plan_remove_missing` continue to drive the runner path.

- **Update** `plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing`
  (lines 2068-2106): same scenario, but the `assert_eq!` now expects
  the new hot-unplug wording verbatim. The `!msg.contains("no missing
  devices detected")` negation stays.
- **Add** four unit tests against `validate_missing_id_target`
  directly, all using hand-constructed `PoolState`:
  1. `validate_missing_id_target_live_rejected`: devid in `pool.devices`.
  2. `validate_missing_id_target_authoritative_missing_accepted`: devid
     only in `pool.missing_devids` -- returns `Ok(())`.
  3. `validate_missing_id_target_null_underlying_only_rejected`:
     devid only in `pool.null_underlying`, asserts the new hot-unplug
     wording verbatim.
  4. `validate_missing_id_target_missing_and_null_underlying_accepted`:
     devid in *both* `pool.missing_devids` and `pool.null_underlying`
     -- returns `Ok(())`. This is the overlap regression guard the
     reviewer surfaced: it cannot be reached through `probe_pool` with
     the current parser, but the helper must still handle it
     correctly so `alert_missing_devids`'s BTreeSet dedup and the
     authority order remain consistent.
- **Untouched:** `plan_remove_missing_rejects_wrong_missing_id_from_pool_state`
  (line 1188 onward). It exercises a devid that is in *neither*
  `missing_devids` nor `null_underlying`; the generic wording at line
  1218 stays accurate. The accompanying "must not call
  BtrfsDeviceUsageRaw" assertion (line 1222-1228) remains valid because
  the new code still reads from the probe-built `pool`, not a fresh
  probe.

### 2. `cli/src/replace.rs`

**Restructure `resolve_replace_source` (lines 1511-1608)** to read
authoritative state from `pool`:

- **Remove** the `let missing_devids = preflight::probe_missing_devids(
  runner, mount_point)?;` call (lines 1559-1560).
- Inside the function, introduce a small helper closure (or local fn) to
  build the null-underlying refusal once:
  ```rust
  let null_underlying_refusal = |devid: u64| ReplaceError::Validation(format!(
      "devid {id} is hot-unplugged but btrfs has not yet promoted it \
       to MISSING (LUKS mapper open, backing device gone). \
       `braid replace` only operates on btrfs-authoritative MISSING \
       devids. Confirm the disk is truly gone, then relock and \
       re-unlock the pool degraded (`braid lock` then `braid unlock \
       --allow-degraded`) so btrfs promotes devid {id}, and retry.",
      id = devid,
  ));
  ```
- **`--missing-id` supplied branch (currently lines 1562-1583)**:
  after the live-device check, add the null-underlying check ahead of
  the existing missing-set refusal:
  ```rust
  if !pool.missing_devids.contains(&supplied) {
      if pool.null_underlying.iter().any(|d| d.devid == supplied) {
          return Err(null_underlying_refusal(supplied));
      }
      return Err(ReplaceError::Validation(format!(
          "devid {supplied} is not a missing device in this pool. \
           Use 'braid status' to see device IDs."
      )));
  }
  ```
- **Auto-resolve branch (currently lines 1584-1604)**: insert the
  null-underlying check *before* both pre-existing refusals so the
  hot-unplug wording wins over either generic message:
  ```rust
  if !pool.missing_devids.contains(&persisted_devid) {
      if pool.null_underlying.iter().any(|d| d.devid == persisted_devid) {
          return Err(null_underlying_refusal(persisted_devid));
      }
      if pool.missing_devids.is_empty() {
          return Err(ReplaceError::Validation(format!(
              "disk '{}' not found in pool and no missing devices detected.",
              old_name
          )));
      }
      return Err(ReplaceError::Validation(format!(
          "disk '{}' records devid {} in pool.json, but btrfs reports \
           it is not missing. Pool membership may be out of date; run \
           `braid status` to inspect.",
          old_name, persisted_devid
      )));
  }
  persisted_devid
  ```

Authority order applied: live -> overlap-allowed `missing_devids` ->
null-underlying-only -> generic.

`preflight::probe_missing_devids` itself is **not** removed -- it still
has a caller at `cli/src/doctor.rs:611`. Only `replace.rs`'s use goes
away; drop the import if it becomes unused.

The `runner` and `mount_point` parameters on `resolve_replace_source`
become unused inside the function. Leaving them in keeps the diff
small; the seven callers (one production at `replace.rs:1155`, six
tests) still type-check. A signature-slim cleanup is **out of scope**
for this plan (separate followup if desired).

**Test changes in `cli/src/replace.rs`:**

Audit of every current `resolve_replace_source` call site, with
whether the test's scenario reaches the missing-set check post-rewrite
(grep: `grep -n "resolve_replace_source(" cli/src/replace.rs`):

| Line | Test                                              | Reaches missing-set? | Action            |
|------|---------------------------------------------------|----------------------|-------------------|
| 1155 | `plan_replace` production call                    | n/a                  | -                 |
| 2087 | `live_old_resolution_succeeds_no_missing`         | No (live arm)        | -                 |
| 2110 | `live_old_with_missing_id_rejects`                | No (live arm)        | -                 |
| 2136 | `live_old_with_pool_missing_rejects`              | No (live arm, `missing_count > 0` branch) | -        |
| 2392 | `dead_old_resolution_single_missing`              | Yes (auto-resolve)   | **Update fixture** |
| 2420 | `dead_old_resolution_with_devid`                  | Yes (--missing-id)   | **Update fixture** |
| 2457 | `missing_id_pointing_to_live_device_rejected`     | No (live-devid check fires first) | -    |
| 2488 | `missing_id_disagrees_with_persisted_devid`       | No (`OldDevidMismatch` fires first) | -    |
| 2527 | `persisted_devid_not_in_missing_set_rejected`     | Yes (auto-resolve, expects stale-pool message) | **Update fixture** |
| 2564 | `missing_path_without_persisted_devid_rejected`   | No (`OldMemberMissingDevid` early return) | - |
| 4731 | decoy regression test (`misleading-label`)        | Yes, **already sets `pool.missing_devids = vec![2]`** | - |
| 4829 | `replace_live_observed_mapper_journaling_regression` | No (live arm)     | -                 |

**Three tests genuinely need fixture updates** -- 2392, 2420, and
2527. Each must add the appropriate value to `pool.missing_devids`:

- `dead_old_resolution_single_missing` (2384): `pool.missing_devids = vec![2]`.
- `dead_old_resolution_with_devid` (2413): `pool.missing_devids = vec![2]`.
- `persisted_devid_not_in_missing_set_rejected` (2520): `pool.missing_devids = vec![3]`.
  Without this, the rewrite would route the test into the "no missing
  devices detected" branch instead of the "Pool membership may be out
  of date" branch it asserts on -- silently the wrong refusal.

After the rewrite, the runner argument inside these three tests no
longer affects the outcome. `mock_with_missing_devids` becomes dead
code if no other caller in the file uses it -- drop the helper in
that case (Read it back with `grep mock_with_missing_devids
cli/src/replace.rs` after the test edits).

- **Add** three new tests:
  1. `missing_id_null_underlying_refused`: `--missing-id N`, `N` in
     `pool.null_underlying`, `N` *not* in `pool.missing_devids`. Asserts
     the new hot-unplug wording verbatim.
  2. `auto_resolve_null_underlying_refused`: no `--missing-id`,
     persisted devid `N` is in `pool.null_underlying`,
     `pool.missing_devids` is empty. Same assertion.
  3. `missing_id_in_both_missing_and_null_underlying_proceeds`: devid
     `N` populated in *both* `pool.missing_devids` and
     `pool.null_underlying`; `--missing-id N`. Asserts the result is
     `Ok(ReplaceSource::Missing { devid: N })` -- regression guard
     against the disjointness misconception that an earlier draft of
     this plan encoded.
  All three follow the Intent / Why / Scenario test-preamble convention
  (AGENTS.md "Test Conventions").

### 3. Manual

#### `manual/commands/remove-missing.md:16-26`

Replace the 11-line workaround paragraph with a short factual note:

```markdown
Note: `braid remove-missing` operates only on btrfs-authoritative `MISSING`
devids. A drive that is hot-unplugged while the pool is mounted
contributes to `missing_count` and appears in `missing_devids` in
`braid status` before btrfs promotes its devid to `MISSING`;
`remove-missing` refuses the devid with a specific hot-unplug
diagnostic until that promotion happens. See
[Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).
```

#### `manual/commands/replace.md:28-38`

Same shape, swapping the command name:

```markdown
Note: `braid replace` operates only on btrfs-authoritative `MISSING`
devids. A drive that is hot-unplugged while the pool is mounted
contributes to `missing_count` and appears in `missing_devids` in
`braid status` before btrfs promotes its devid to `MISSING`; both
`replace --missing-id N` and the no-flag auto-resolve path refuse the
devid with a specific hot-unplug diagnostic until that promotion
happens. See
[Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).
```

#### `manual/guides/recovery-scenarios.md`

Insert a new `### Hot-unplug while pool is mounted` subsection inside
the existing `## Missing disk (drive failure)` section, between
`### Unlock with a missing disk` (line 223) and `### Option A: Replace
the disk` (line 233). Use `braid lock` for the relock step -- raw
`umount` skips the scoped `btrfs device scan --forget` that
`lock.rs:538-573` performs after umount, leaving stale kernel
references on multi-device pools.

```markdown
### Hot-unplug while pool is mounted

If a drive is physically disconnected while the pool is mounted, its LUKS
mapper can remain open with `cryptsetup status` reporting `device: (null)`.
btrfs continues to list the devid but has not yet promoted it to MISSING.
`braid status` reports the devid -- it contributes to `missing_count` and
appears in `missing_devids` -- but `braid remove-missing --missing-id N`
and `braid replace` (with or without `--missing-id`) refuse the devid
because they only act on btrfs-authoritative MISSING entries.

To make progress:

1. Confirm the disk is truly gone (not just a loose cable).
2. Relock and re-unlock the pool degraded so btrfs re-evaluates membership
   and promotes the devid:
   ```sh
   sudo braid lock
   sudo braid unlock --allow-degraded
   ```
3. Re-run `braid status` -- the devid should now appear as authoritatively
   MISSING -- then retry `braid remove-missing` or `braid replace`.
```

## Critical files

- `cli/src/remove_missing.rs:380-398` -- collapse live/missing-id
  validation into a `validate_missing_id_target(&PoolState, u64)`
  helper call.
- `cli/src/remove_missing.rs` -- new private `validate_missing_id_target`
  helper, placed near the existing per-id validation.
- `cli/src/remove_missing.rs:2068-2106` -- existing null-underlying
  integration test to update; four new helper-unit tests (live,
  authoritative-missing-accepted, null-underlying-only,
  missing+null-underlying-overlap-accepted) added nearby.
- `cli/src/replace.rs:1511-1608` -- rewrite `resolve_replace_source`
  onto `pool`, introduce inner refusal helper.
- `cli/src/replace.rs` test module -- update three dead-path tests
  (`dead_old_resolution_single_missing` 2384, `dead_old_resolution_with_devid`
  2413, `persisted_devid_not_in_missing_set_rejected` 2520) to set
  `pool.missing_devids` directly; add three new tests for
  null-underlying refusal (`--missing-id`, auto-resolve) and
  overlap-proceeds. See the caller-audit table above for why the other
  eight `resolve_replace_source` call sites (2087/2110/2136/2457/2488/2564/4731/4829)
  need no fixture update.
- `cli/src/types.rs:354-393` -- read-only reference; the overlap case
  is documented in the `alert_missing_devids` doc comment.
- `cli/src/probe.rs:380-437` -- read-only reference for how
  `devices` / `null_underlying` are populated (note: `devices` and
  `null_underlying` are populated mutually exclusively, but
  `null_underlying` and `missing_devids` are not -- btrfs can promote
  the devid while the mapper is still open).
- `cli/src/lock.rs:538-573` -- read-only reference for why the recovery
  guide says `braid lock` rather than `umount`.
- `manual/commands/remove-missing.md:16-26` -- prune workaround.
- `manual/commands/replace.md:28-38` -- prune workaround.
- `manual/guides/recovery-scenarios.md:223-232` -- insert new
  subsection between "Unlock with a missing disk" and "Option A:
  Replace the disk".

## Reused functions / patterns

- `PoolState.null_underlying` and `PoolState.missing_devids`
  (`cli/src/types.rs:354-374`) -- the authoritative sets we
  discriminate on. **Not disjoint:** see the `alert_missing_devids`
  doc comment at `types.rs:378-383` for the overlap rationale.
- `PlanFailure::with_notes` / `ReplaceError::Validation` -- existing
  error constructors; no new variants needed.
- Test-preamble convention (`Intent` / `Why it exists` / `Scenario`) --
  AGENTS.md "Test Conventions".
- CLI message style: bare `--`, command-name references
  (`'braid status'`, `'braid lock'`), no embedded URLs -- AGENTS.md
  "CLI Output Style" plus the `docs/luks-unlock.md` messaging
  invariant.

## Verification

1. **Rust unit tests:** `just test-rust`
   - Updated `plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing`
     asserts the new wording via the integration path.
   - Four new `remove_missing.rs` helper-unit tests against
     `validate_missing_id_target` cover live, authoritative-missing
     accept, null-underlying-only reject, and the
     missing+null-underlying overlap accept (the overlap case is only
     reachable as a unit test -- see the helper rationale in the code
     changes section).
   - New `replace.rs` tests cover `--missing-id` null-underlying
     refusal, auto-resolve null-underlying refusal, and the overlap-
     proceeds case (these are unit-testable directly because
     `resolve_replace_source` takes `&PoolState`).
   - Updated dead-path tests in `replace.rs` populate
     `pool.missing_devids` directly.
   - `plan_remove_missing_rejects_wrong_missing_id_from_pool_state`
     and `missing_id_pointing_to_live_device_rejected` continue to
     pass with their original messages (different branches in the
     chain).
2. **No new VM test required.** This is a planning-time error-text
   change with no runtime side effects, fully reachable through the
   existing plan-only Rust tests under `MockRunner`. The existing
   `tests/` VM suite for remove-missing / replace remains valid.
3. **Manual smoke:** verify the anchor
   `#hot-unplug-while-pool-is-mounted` resolves from both manual
   pages (markdown auto-generates anchors from headings, so the slug
   must match the heading text exactly).
4. **Authority-order spot-check (read-only):** confirm via
   `cli/src/types.rs:378-383` and `cli/src/probe.rs:380-437` that
   `pool.missing_devids` and `pool.null_underlying` can intersect, and
   that the new ordering (live -> missing_devids -> null_underlying ->
   nowhere) treats the intersection correctly.

## Out of scope (deliberately)

- `cli/src/doctor.rs:611` also calls `probe_missing_devids` rather than
  reading `pool.missing_devids`. That site has no `PoolState` in
  scope; rerouting it would require a separate probe-plumbing change.
- Slimming `resolve_replace_source`'s signature to drop the now-unused
  `runner` and `mount_point` parameters. The function stays generic
  over `R: CommandRunner` to minimize the call-site diff; a follow-up
  can clean this up.
- Renaming `PoolState.missing_devids` (btrfs-only) vs.
  `StatusReport.missing_devids` (alert union). The collision is real
  and `cli/src/status.rs:73-80` documents it; renaming is a separate
  cross-cutting change.
