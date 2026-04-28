# Three-layer acked-stats hygiene: add-time guard + remove-time prune + monitor reconcile

## Context

`cli/src/monitor.rs:72-87` self-heals `missing_acked = false` for devids that have reappeared, but never deletes acked-stats entries whose devid has permanently left the pool (via `braid remove` or `braid remove-missing`). `cli/src/ack.rs:61` (`snapshot_current`) does collect the garbage by rewriting the file from scratch, but many monitor cycles run between acks.

This is **not purely cosmetic**: btrfs allocates new add-device ids as `last_devid + 1` (kernel: `reference/linux/fs/btrfs/volumes.c:1903` -- `*devid_ret = found_key.offset + 1`). Devid reuse therefore happens only when the **current maximum** devid is removed and a new disk is then added (e.g. remove devid 4 from a `{1,2,3,4}` pool, then add -> new disk gets devid 4). Removing a non-max devid leaves a permanent gap; the next add still gets `max + 1`. The fix protects against the reuse case, which is realistic on small NAS pools where the user typically replaces disks at the tail of the devid sequence. If a stale acked-stats entry survives the remove for the reused devid, its `device_stats` baseline carries onto the new device:

- `has_new_errors` (alert.rs:135-171) compares current counters against the acked baseline. A stale baseline of `read_io_errs = 5` suppresses alerts on the new device until its counters exceed that ghost baseline.
- A stale `missing_acked = true` (e.g., from a `remove-missing` orphan) suppresses `MissingDevice` alerts if the new device's underlying ever drops.

Once the devid is reused, monitor's reconcile path **cannot** detect the staleness on its own: the reused devid is in `still_relevant`, so the entry looks valid to monitor. The correctness boundary is therefore **at add-time**, when we know a devid was just assigned to a fresh disk and any pre-existing acked entry for it must be ghost data.

Note on scope: the original issue text lists `replace` as an orphan source, but `btrfs replace` preserves the devid in-place (`replace.rs:1002` "will be replaced in-place"). Only `remove` / `remove-missing` retire devids and only `add` introduces new ones.

Severity: bumped from Low (cosmetic) to **potential health-alert suppression on devid reuse**.

Intended outcome -- three layers, ordered by criticality:

1. **Correctness boundary -- `braid add`:** after btrfs assigns a devid to each newly added disk, drop any pre-existing acked-stats entry for that devid. This is the only layer that handles stale entries left by older braid versions, manual `btrfs device remove`, or a failed remove-time cleanup write.
2. **Hygiene -- `braid remove` / `braid remove-missing`:** drop the affected devid's acked-stats entry on success. Closes the race for normal flows so an immediate `add` doesn't even need to rely on the add-time guard.
3. **Defense-in-depth -- `braid monitor`:** every cycle, prune orphan entries (devid no longer in pool at all) and self-heal `missing_acked` for reappeared devices. Handles crash recovery, manual btrfs operations, and any other path that left a true orphan.

## Approach

Three insertion layers, backed by helpers in `alert.rs`.

**Helpers in `alert.rs`:**

- `drop_acked_devid(&mut AckedStats, devid: u64) -> bool` -- removes one entry; encapsulates the devid -> string-key encoding.
- `drop_ghost_acked_for_devids(paths: &StatePaths, devids: &[u64]) -> Result<bool, std::io::Error>` -- load, drop each devid via `drop_acked_devid`, save on change. Used by the live-pool add loop and the remove/remove-missing callsites for targeted, devid-scoped cleanup.
- `remove_acked_stats(paths: &StatePaths) -> Result<(), std::io::Error>` -- delete the file outright (NotFound treated as success, matching the existing `remove_alert_latch` pattern at alert.rs:253). Used by the bootstrap path, where every existing entry is guaranteed stale because the pool's identity is new.
- `reconcile_acked_stats(&mut AckedStats, still_relevant, present) -> bool` -- one `BTreeMap::retain` pass that drops orphan keys and clears self-healed `missing_acked`.

Unparsable keys are kept (matches today's `if let Ok(devid)` defensive behavior -- never silently delete data we don't understand).

`alert.rs` is the right home: `AckedStats` lives there, `snapshot_current` (the fresh-build companion for `cmd_ack`) lives there, and the existing test fixtures in `alert.rs:314+` make unit tests trivial.

Failure-mode policy by layer:

- **Add-time cleanup failure is command-fatal.** Add-time is the correctness boundary; if cleanup fails after the btrfs mutation succeeded, returning success would let the user trust health monitoring on a pool with a known stale baseline. Instead, return an explicit error variant whose message names exactly what happened (pool was modified, btrfs add succeeded, ack-stats cleanup failed for devid X, manual repair required: `rm /var/lib/braid/acked-stats.json` before trusting alerts). Inside the live-pool add loop, abort on the first cleanup failure rather than introducing additional disks.
- **Remove-time cleanup failure is non-fatal warning.** The next add for that devid will catch it via the add-time guard. Print a warning and continue.
- **Monitor reconcile failure is non-fatal warning.** Defense-in-depth only; the upper layers are responsible for correctness.

## Changes

### `cli/src/alert.rs`

Add four public functions next to `snapshot_current`:

```rust
/// Drop the acked entry for `devid`. Returns true if an entry was removed
/// (caller should persist). Encapsulates the devid -> string-key encoding
/// so callers don't reach into `acked.0` directly.
pub fn drop_acked_devid(acked: &mut AckedStats, devid: u64) -> bool {
    acked.0.remove(&devid.to_string()).is_some()
}

/// Fallible variant of `load_acked_stats` that distinguishes "no file"
/// (treat as empty) from "file unreadable / corrupt" (propagate). The
/// existing infallible `load_acked_stats` swallows both into an empty
/// default, which is acceptable for monitor's defense-in-depth path but
/// dangerous on the add-time correctness boundary -- a read or parse
/// failure there must abort, not silently report "no ghosts found."
fn load_acked_stats_fallible(paths: &StatePaths) -> Result<AckedStats, std::io::Error> {
    let path = paths.acked_stats_json();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AckedStats::default());
        }
        Err(e) => return Err(e),
    };
    serde_json::from_str(&contents).map_err(std::io::Error::other)
}

/// Load acked-stats, drop entries for each devid in `devids`, and persist
/// on change. Returns Ok(true) if the file was rewritten, Ok(false) if no
/// entry matched (no-op, no I/O write). Used by the live-pool add loop
/// and the remove/remove-missing callsites.
///
/// Read errors and JSON parse errors are propagated (not swallowed): the
/// add-time correctness boundary requires that we either prove no ghost
/// exists or surface the error to the user; silently reporting "no ghost"
/// when the file is unreadable would let `cmd_add` return success on a
/// pool whose ack state may be stale.
///
/// Does no work if `devids` is empty (returns Ok(false) without reading
/// the file).
pub fn drop_ghost_acked_for_devids(
    paths: &StatePaths,
    devids: &[u64],
) -> Result<bool, std::io::Error> {
    if devids.is_empty() {
        return Ok(false);
    }
    let mut acked = load_acked_stats_fallible(paths)?;
    let mut changed = false;
    for &devid in devids {
        changed |= drop_acked_devid(&mut acked, devid);
    }
    if changed {
        save_acked_stats(&acked, paths)?;
    }
    Ok(changed)
}

/// Delete `acked-stats.json` outright. NotFound is treated as success.
/// Used by the bootstrap path: a fresh pool's identity is new, so every
/// pre-existing entry is guaranteed-stale regardless of devid. Mirrors
/// the existing `remove_alert_latch` pattern (alert.rs:253).
pub fn remove_acked_stats(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.acked_stats_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Reconcile acked-stats with current pool state during a monitor cycle.
/// Returns true if the map changed (caller should persist).
///
/// - Drops entries whose devid is no longer in the pool at all (orphans
///   from a remove that crashed before its own cleanup, or btrfs ops
///   performed outside braid).
/// - Clears `missing_acked` for entries whose devid is now present
///   (self-heal: a missing-acked device reappeared).
pub fn reconcile_acked_stats(
    acked: &mut AckedStats,
    still_relevant: &BTreeSet<u64>,
    present: &BTreeSet<u64>,
) -> bool {
    let mut changed = false;
    acked.0.retain(|key, disk| {
        let Ok(devid) = key.parse::<u64>() else {
            return true;
        };
        if !still_relevant.contains(&devid) {
            changed = true;
            return false;
        }
        if disk.missing_acked && present.contains(&devid) {
            disk.missing_acked = false;
            changed = true;
        }
        true
    });
    changed
}
```

### `cli/src/add.rs` (correctness boundary)

`cmd_add` has two device-introduction paths. Both must call the cleanup helper.

**Bootstrap path (initial pool creation, lines 587-607).** Bootstrap creates a brand-new btrfs filesystem -- the pool's identity is new, so every entry already in `acked-stats.json` is guaranteed-stale, regardless of which devids it references. The cleanup must NOT depend on a successful post-mount probe (if `enrich_from_live_pool` fails, ghost data on disk would still survive). Insert immediately after `pool_bootstrap_mount` / `pool_bootstrap_mount_raid1` succeeds (line ~590 or ~594), BEFORE `enrich_from_live_pool`:

```rust
// Fresh pool: any acked-stats entries belong to a previous, wiped pool.
// Clear unconditionally before enrichment so a probe failure does not
// leave ghost baselines for whatever devids btrfs reuses. Cleanup
// failure here is COMMAND-FATAL: btrfs has already mutated, so we
// cannot return success with a known-stale acked-stats file.
alert::remove_acked_stats(params.paths)
    .map_err(|e| AddError::AckCleanupFailed {
        stage: "bootstrap",
        detail: e.to_string(),
    })?;
```

The new `AddError::AckCleanupFailed` variant carries an explicit message: pool was created/mounted successfully, but ack-stats cleanup failed; user must `rm /var/lib/braid/acked-stats.json` before trusting `braid monitor`.

**Live-pool add loop (lines 609-613).** Cleanup runs **inside** the loop, after each successful `pool_add_device`, so partial multi-add (some disks succeed, a later one fails) still cleans up the disks that were introduced before the failure point. Both probe failure and ack-stats write failure are **command-fatal** -- the disk is in the pool, the correctness boundary has been breached, and continuing the loop would compound the problem:

```rust
for mp in &mapper_paths {
    pool_add_device(runner, mp, mount_point)?;
    eprintln!("Device added to pool: {}", mp);
    // Probe just enough to learn the devid btrfs assigned to this disk.
    // Cost: N probes for an N-disk add (acceptable for a rare command).
    let pool_after = probe_pool(runner, mount_point)
        .map_err(|e| AddError::AckCleanupFailed {
            stage: "post-add probe",
            detail: format!("{mp}: {e}"),
        })?;
    let devid = devid_for_mapper_path(&pool_after, mp)
        .ok_or_else(|| AddError::AckCleanupFailed {
            stage: "post-add probe",
            detail: format!("{mp}: not found in pool after add"),
        })?;
    alert::drop_ghost_acked_for_devids(params.paths, &[devid])
        .map_err(|e| AddError::AckCleanupFailed {
            stage: "live-pool add",
            detail: format!("devid {devid}: {e}"),
        })?;
}
```

`devid_for_mapper_path` is a small inline helper that strips `/dev/mapper/` from `mp` and matches against `pool_after.devices[i].mapper.0`. If `add.rs` already has such a helper (check before adding -- look near `enrich_from_live_pool` at line 286), reuse it.

The post-loop `enrich_from_live_pool` call at line 619 stays untouched -- it serves the membership-population purpose, separate from acked cleanup.

`AddError::AckCleanupFailed { stage, detail }` is a new variant. Its `Display` impl spells out: btrfs add succeeded for the disk, but acked-stats cleanup failed at `<stage>` (`<detail>`); pool is mutated; user must `rm /var/lib/braid/acked-stats.json` before trusting `braid monitor`.

Remaining gap (acceptable, pre-existing): a `pool_add_device` call that hangs or panics mid-call (not a clean error return) leaves the new disk in the pool without cleanup running.

### `cli/src/remove.rs` (hygiene)

In `RemovePlan::execute` (line 114), after `journal::clear_journal(params.paths)?` (line ~200) and before the final `eprintln!("Done. ...")`, call the centralized helper:

```rust
if let Err(e) = alert::drop_ghost_acked_for_devids(params.paths, &[self.target_devid]) {
    eprintln!("Warning: failed to update acked stats: {e}");
}
```

`self.target_devid` is already in scope (RemovePlan struct, line 82). `params.paths: &StatePaths` is already in scope. Failure is non-fatal -- the next add for this devid will catch it via the add-time guard.

### `cli/src/remove_missing.rs` (hygiene)

In `RemoveMissingPlan::execute` (line 113), after `journal::clear_journal(params.paths)?` (line ~213) and before the final `eprintln!`, the same call with `self.missing_id` (RemoveMissingPlan, line 80) instead of `target_devid`.

### `docs/decisions/014-alerts.md` (ADR update)

ADR is `Active` (line 3) and already documents devid-keyed ack state (lines 41-44, "Ack state keyed by btrfs devid"). Add a new subsection after "Corrupt latch recovery" (after line 95), before "## Rejected alternatives" (line 97):

```markdown
### Acked-stats hygiene across pool membership changes

btrfs allocates new devids as `last_devid + 1` (kernel: `fs/btrfs/volumes.c`, `find_next_devid`), so a `remove`-then-`add` sequence reuses the removed devid only when that devid was the current maximum at remove time. (Removing a non-max devid leaves a permanent gap.) A stale acked-stats entry for a reused devid would otherwise carry the previous holder's `device_stats` baseline (suppressing health alerts until counters exceed the ghost) or its `missing_acked = true` flag (suppressing missing-device alerts) onto the fresh disk.

Invariant: a reused devid must never inherit the previous holder's ack baseline.

Three layers enforce it:

1. **Add-time guard (correctness boundary):** `cmd_add` clears acked-stats unconditionally on bootstrap (every existing entry is stale because the pool's identity is new) and drops the assigned devid's entry per-disk inside the live-pool add loop (so partial multi-add still cleans up the disks that were introduced before a later failure). Cleanup failure here is **command-fatal**: returning success with a known stale baseline would let the user trust health monitoring on a pool the alert layer cannot reason about. The error names the stage and instructs the user to delete the file before relying on alerts.
2. **Remove-time prune (hygiene):** `cmd_remove` and `cmd_remove_missing` drop the affected devid's acked-stats entry on success. Cleanup failure here is non-fatal (warning) -- the next `add` for that devid will catch it via layer 1.
3. **Monitor reconcile (defense-in-depth):** `cmd_monitor` prunes orphan entries (devid no longer in `pool.devices ∪ pool.null_underlying ∪ pool.missing_devids`) every cycle. Catches crash recovery and manual btrfs operations performed outside braid. Cannot detect ghost data once a devid is reused -- the add-time layer is the boundary for that case.
```

### `cli/src/monitor.rs`

Replace lines 72-87 with the helper call. Build `still_relevant` and `present` from `pool` once, call the helper, persist on change:

```rust
// 7. Self-heal: clear stale missing_acked, drop orphan acked entries.
let still_relevant: BTreeSet<u64> = pool
    .devices.iter().map(|d| d.devid)
    .chain(pool.null_underlying.iter().map(|d| d.devid))
    .chain(pool.missing_devids.iter().copied())
    .collect();
let present: BTreeSet<u64> = pool.devices.iter().map(|d| d.devid).collect();

if alert::reconcile_acked_stats(&mut acked, &still_relevant, &present)
    && let Err(e) = save_acked_stats(&acked, paths)
{
    eprintln!("Warning: failed to update acked stats: {e}");
}
```

The existing `let present_devids: Vec<u64> = ...` (line 75) is replaced by the `BTreeSet` above. No other behavior changes -- the rest of `cmd_monitor` is untouched.

## Tests

Each new test gets the AGENTS.md `/* Intent / Why it exists / Scenario */` block-comment preamble per `feedback_test_preamble_block_comment_literal.md`.

**Note on existing VM coverage:** `tests/cli/braid-monitor.py` only covers the forward path (close mapper -> ack -> cleared). It never reopens a closed mapper or removes a disk, so it does NOT exercise self-heal or orphan scenarios today.

### Primary correctness regression: cmd_add (six tests covering both paths)

These tests must drive cmd_add through the full success path so the new cleanup callsites actually execute. The existing `AddRecordingRunner` (and similar test runners in `add.rs`) abort earlier in the flow (e.g. at LUKS header backup) and would never reach `pool_add_device` or the post-add probe; using it as-is would silently make the new tests vacuous. Either extend `AddRecordingRunner` to a full-path mode that completes LUKS setup, runs `pool_add_device`, and serves the post-add `probe_pool` (preferred -- one shared helper), or write a new `AddFullPathRunner` per-test. Verify the test reaches the new cleanup by, e.g., asserting on a side-effect that only happens after `pool_add_device` (a recorded `BtrfsDeviceAdd` request, or the new acked-stats file state). Otherwise pattern matches `add.rs:3018` (`cmd_add_live_pool_fresh_add_single_prompt`): `add_test_setup()` + `AddMockFs` + scripted passphrase.

**Test 1 -- live-pool single add, ghost dropped:**

1. Configure the post-add `probe_pool` mock so the new disk lands at devid 2.
2. Pre-write `acked-stats.json` with a stale entry for devid 2 (`read_io_errs = 5`, `missing_acked = true`) -- ghost from a prior pool generation.
3. Pre-write a control entry for devid 1 (existing pool member, must stay byte-equal).
4. Run `cmd_add` successfully.
5. Assert: entry for devid 2 is gone; entry for devid 1 is byte-equal to seed.

**Test 2 -- bootstrap, ghost cleared even when post-bootstrap probe fails:**

1. Drive `cmd_add` through the bootstrap branch (no existing pool; mkfs + mount path).
2. Pre-write `acked-stats.json` with ghost entries for devids 1, 2, and 7 (left over from a previous wiped pool, including a high devid the new bootstrap won't reuse).
3. Configure the post-bootstrap `probe_pool` mock to **fail** (e.g., return a `ProbeError`). This forces `enrich_from_live_pool` into its non-populating branch -- if cleanup depended on probe success, the ghosts would survive.
4. Run `cmd_add` successfully (bootstrap mount succeeded; enrich failure is non-fatal because `enrich_from_live_pool` swallows probe errors).
5. Assert: `acked-stats.json` does not exist (or, if it exists, is byte-equal to an empty `AckedStats`).

This locks both the bootstrap-path callsite AND the placement of the cleanup BEFORE enrichment. It fails if the cleanup is moved post-enrich, made probe-dependent, or omitted.

**Test 3 -- partial multi-add: succeeded disks cleaned up before failure:**

1. Configure the live-pool `pool_add_device` mock to succeed for disk A and fail for disk B (mid-loop failure).
2. Configure post-add `probe_pool` (called after disk A's add) to report disk A at devid 2.
3. Pre-write `acked-stats.json` with ghost entries for devid 2 (disk A) and devid 3 (disk B).
4. Run `cmd_add`; expect it to error (from disk B's failure).
5. Assert: entry for devid 2 is gone (cleanup ran inside the loop after disk A succeeded); entry for devid 3 is unchanged (disk B never made it into the pool).

This locks the per-iteration placement of the cleanup. It fails if cleanup is moved out of the loop to a single post-loop call.

**Test 4 -- live-pool ack-cleanup write failure is fatal with explicit error:**

1. Configure the live-pool path so `pool_add_device` succeeds and post-add `probe_pool` reports the new disk at devid 2.
2. Make the `acked-stats.json` write fail (e.g., set `paths.acked_stats_json()` to a path under a read-only directory, or inject an I/O failure into the test FS).
3. Run `cmd_add`.
4. Assert: returns `Err(AddError::AckCleanupFailed { stage: "live-pool add", .. })`. Match on the typed variant per `feedback_assert_typed_error_shape_not_substrings.md` -- do not assert on message substrings.

**Test 5 -- bootstrap ack-cleanup failure is fatal with explicit error:**

1. Drive bootstrap; make `remove_acked_stats` fail (e.g., make the file location read-only).
2. Run `cmd_add`.
3. Assert: returns `Err(AddError::AckCleanupFailed { stage: "bootstrap", .. })`.

**Test 6 -- post-add probe failure is fatal with explicit error:**

1. Configure the live-pool path so `pool_add_device` succeeds.
2. Configure the immediate post-add `probe_pool` to fail (e.g., return a `ProbeError`) -- or, alternatively, return a probe whose `pool.devices` doesn't contain a mapper matching the just-added `mp` (covers the "new mapper not found" arm).
3. Run `cmd_add`.
4. Assert: returns `Err(AddError::AckCleanupFailed { stage: "post-add probe", .. })`. Match on the typed variant per `feedback_assert_typed_error_shape_not_substrings.md`.

This locks the post-btrfs-mutation correctness boundary: even when the probe (not the write) fails, the command must surface a typed error rather than warn-and-continue. It fails if the implementation falls back to `eprintln!` warnings on probe failure.

These six together are the **primary correctness regression** for the add-time guard.

### Hygiene regression: cmd_remove + cmd_remove_missing

Add one test to each of `remove.rs` and `remove_missing.rs`, matching the `RecordingRunner` + tempdir pattern at `remove.rs:862` (`two_to_one_remove_invokes_survivor_capacity_preflight`) and `remove_missing.rs:786` (`no_usage_probe_for_single_survivor`).

For each test:

1. Build a 2-disk pool fixture; the disk to be removed has devid 2.
2. `tempfile::tempdir()` -> `StatePaths::custom(...)`.
3. Pre-write `acked-stats.json` with two entries: devid 1 with non-zero counters (control, must remain byte-equal), devid 2 with non-zero counters or `missing_acked = true` (must be removed).
4. Run `cmd_remove` (or `cmd_remove_missing`) successfully.
5. Read `acked-stats.json` back. Assert keys are exactly `{"1"}` and entry 1 is byte-equal to seed.

These fail if the remove-time cleanup is dropped.

### Defense-in-depth: cmd_monitor integration test

In a new `#[cfg(test)] mod tests` in `monitor.rs`. Builds a `MockRunner` (cmd.rs:940) wired through the full `probe_pool` sequence. Probe issues `CryptsetupLuksUuid`, `CryptsetupStatus`, `FindmntJson`, and `BtrfsFilesystemShow` (probe.rs lines 112, 176, 196, 218, 251, 281, 314, 358, 379) -- no lsblk and no LuksDumpText for this pool shape. Pattern to follow: `cli/src/replace.rs:2086` and `FailingReplaceRunner` at `cli/src/replace.rs:1907`. `cmd_monitor` then calls `BtrfsDeviceStatsJson` directly.

Pool fixture covers all three relevance categories so the test can distinguish between candidate unions:

- devid `1`: present (in `pool.devices`).
- devid `2`: **null-underlying** (mapper open, underlying gone -- in `pool.null_underlying`).
- devid `3`: **btrfs-MISSING** (in `pool.missing_devids`).

Single test, exercising prune + self-heal + every union axis in one cycle:

1. tempdir -> `StatePaths::custom(...)`.
2. Pre-write `acked-stats.json` with four entries:
   - devid `1`: `missing_acked = true` (will be self-healed since 1 is now present).
   - devid `2`: `missing_acked = true` (must stay -- null-underlying is still relevant).
   - devid `3`: `missing_acked = true` (must stay -- btrfs-MISSING is still relevant).
   - devid `99`: `missing_acked = false` (orphan, must be dropped).
3. Call `cmd_monitor(&runner, &mp, &paths)`.
4. Read `acked-stats.json` back via `load_acked_stats_at`.
5. Assert: keys are exactly `{"1", "2", "3"}`; entry for `1` has `missing_acked = false`; entries for `2` and `3` are byte-equal to seed (incl. `missing_acked = true`); entry for `99` is gone.

Failure modes the broadened fixture catches:

- Omitting `null_underlying` from `still_relevant` -> devid 2 is dropped -> keys assert fails.
- Omitting `missing_devids` from `still_relevant` -> devid 3 is dropped -> keys assert fails.
- Helper not called -> orphan 99 stays AND devid 1 stays missing_acked -> two asserts fail.
- Save skipped -> persisted file equals the seed -> all post-state asserts fail.

### Supplementary: helper unit tests in `cli/src/alert.rs`

Sit next to existing alert.rs tests (line 314+). Cover edge cases the integration tests don't naturally probe; reuse the `tempfile::tempdir()` + `StatePaths::custom` pattern already in use there for the I/O-touching tests.

1. **`drop_acked_devid` returns false on missing devid** -- locks the `is_some()` branch so the caller's `if changed { save }` gate doesn't write unnecessarily.
2. **`drop_ghost_acked_for_devids` empty input is a no-op** -- pass `&[]`, returns `Ok(false)`, file is not touched (assert no `acked-stats.json` was written if none existed before).
3. **`drop_ghost_acked_for_devids` no-match is a no-op** -- seed file with devid 1, call with `&[99]`, returns `Ok(false)`, file mtime / contents unchanged byte-for-byte.
4. **`drop_ghost_acked_for_devids` removes matched entries and persists** -- seed with devids 1, 2, 3, call with `&[2, 3]`, returns `Ok(true)`, file now has only devid 1 byte-equal to seed.
5. **`drop_ghost_acked_for_devids` treats missing file as empty** -- no `acked-stats.json` exists, call with `&[1]`, returns `Ok(false)`, no file is created.
6. **`drop_ghost_acked_for_devids` propagates parse error** -- write garbage bytes to `acked-stats.json`, call with `&[1]`, asserts `Err(io::Error)` (locks the fallible-load behavior; defends the add-time boundary).
7. **`remove_acked_stats` deletes existing file** -- seed any non-empty file, call helper, assert file does not exist.
8. **`remove_acked_stats` is idempotent on NotFound** -- call helper on a path with no file, returns `Ok(())`.
9. **`reconcile_acked_stats` keeps unparsable keys** -- `acked` has key `"garbage"` -> returns `false`, key kept (locks the defensive `if let Ok` branch).
10. **`reconcile_acked_stats` returns false on no-op** -- fully synced state -> returns `false`. Guards against unnecessary disk writes from the `if changed { save }` gate at the call site.

Orphan-drop, self-heal, null-underlying-kept, and btrfs-MISSING-kept are all covered by the integration tests, so no duplication at the unit layer.

### VM test: skipped

A `braid-monitor-prune-orphans.py` is defensible but not required. The cmd_remove / cmd_remove_missing tests bind to the on-disk artifact at the failure layer, and the cmd_monitor integration test locks the defensive path. VM coverage would not add a distinct failure mode the Rust tests miss.

## Critical files

- `cli/src/alert.rs` -- add `drop_acked_devid` + `load_acked_stats_fallible` (private) + `drop_ghost_acked_for_devids` + `remove_acked_stats` + `reconcile_acked_stats` + supplementary unit tests.
- `cli/src/add.rs` -- cleanup at bootstrap success and per-disk inside the live-pool add loop; new `AddError::AckCleanupFailed { stage, detail }` variant; six regression tests (correctness boundary). Test runner setup must drive cmd_add through the full success path -- the existing `AddRecordingRunner` aborts earlier and won't reach the new callsites.
- `cli/src/remove.rs` -- post-success acked-stats cleanup; new regression test.
- `cli/src/remove_missing.rs` -- same as remove.rs.
- `cli/src/monitor.rs` -- replace lines 72-87 with helper call; new `#[cfg(test)] mod tests` for the defense-in-depth integration test.
- `docs/decisions/014-alerts.md` -- new "Acked-stats hygiene across pool membership changes" subsection after line 95.
- `cli/src/types.rs` -- read-only reference for `PoolState` field semantics (no change).

## Verification

1. `just test-rust` -- runs all new tests (cmd_add regression, cmd_remove regression, cmd_remove_missing regression, cmd_monitor integration, helper units) plus existing alert / add / remove / remove_missing / monitor tests.
2. `just test-vm braid-monitor` -- confirms no regression in the existing forward-path ack lifecycle.
3. Manual sanity (optional): on a live VM, trigger an error, `braid ack`, then `braid remove` the **current-maximum devid** (e.g. devid 4 in a `{1,2,3,4}` pool -- removing a non-max devid leaves a gap and the next `add` won't reuse the slot, so the test wouldn't exercise the reuse path). Then `cat /var/lib/braid/acked-stats.json` -- removed devid should already be gone, proving remove-time prune fired. Then `braid add <new disk>` -- the new disk reuses the just-removed max devid; confirm no ghost entry for that devid in `acked-stats.json`.

No fixture refresh needed (no parser-critical tool versions involved).
