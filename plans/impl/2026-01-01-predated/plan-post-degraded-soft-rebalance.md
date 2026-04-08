# Plan: Post-degraded soft RAID1 rebalance

## Context

When btrfs is mounted with `-o degraded` (missing disk), new writes create **single-profile chunks** with zero redundancy — a known, unfixed btrfs bug (proven by existing repro `tests/repro/degraded-writes-single.py`). After `btrfs device remove missing` or `btrfs replace start` completes, those chunks persist. Btrfs docs recommend running `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft` after degraded operations. The `,soft` flag converts only non-RAID1 chunks, skipping already-RAID1 data.

Currently neither `braid remove-missing` nor `braid replace` (missing path) runs this rebalance. `braid doctor` detects the resulting mixed profiles but requires manual follow-up. This change aligns braid with btrfs recommendations.

## Core design: shared helper with correct gate

One shared `maybe_restore_raid1()` helper in `pool.rs`, used by both commands. The gate is: **only run when the operation transitions the pool from degraded to non-degraded (clears the last missing device) AND ≥2 devices are present.** This prevents running the balance while the pool is still degraded (other devices still missing).

**Why this gate is correct:**
- `remove-missing` without `--missing-id`: enforced to have exactly 1 missing (line 69-74), so removal always clears last missing.
- `remove-missing --missing-id N`: could target 1 of several missing. Only rebalance if that was the last one.
- `replace` (missing path): replaces 1 missing device. Only rebalance if no other devices are still missing.
- In all cases: re-probe after the operation to check `pool_after.missing_count == 0 && pool_after.devices.len() >= 2`.

**Ordering:** operation → disk-map update → `maybe_restore_raid1()` → done. Disk-map persists before the balance so that a balance failure doesn't leave stale state.

## Changes (ordered for minimal breakage)

### 1. `cli/src/cmd.rs` — new `BtrfsBalanceRaid1Soft` variant

Add enum variant after `BtrfsBalanceRaid1` (~line 80):
```rust
BtrfsBalanceRaid1Soft { mount_point: String },
```

Add `to_argv()` arm after the `BtrfsBalanceRaid1` arm (~line 337):
- args: `["balance", "start", "-dconvert=raid1,soft", "-mconvert=raid1,soft", mount_point]`

**Test:** `btrfs_balance_raid1_soft_generates_correct_argv` — verify `to_argv()` produces exact `,soft` flags.

### 2. `cli/src/pool.rs` — new `pool_balance_raid1_soft()` + shared `maybe_restore_raid1()`

**2a. `pool_balance_raid1_soft()`** — same pattern as `pool_balance_raid1`, wraps `BtrfsBalanceRaid1Soft`. Progress polling reuses existing `BtrfsBalanceStatus` (btrfs reports progress identically for soft balances).

**2b. `maybe_restore_raid1()`** — shared helper:
```rust
/// Run a soft RAID1 rebalance if the operation just transitioned the pool from
/// degraded to non-degraded with ≥2 present devices. This restores redundancy
/// for single-profile chunks created during degraded operation (known btrfs bug).
///
/// Callers: `remove-missing` and `replace` (missing path), after their primary
/// operation and disk-map update have completed.
pub fn maybe_restore_raid1<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    pre_op_missing_count: u64,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    if pre_op_missing_count == 0 {
        return Ok(()); // Pool wasn't degraded — nothing to restore
    }
    let pool_after = probe_pool(runner, mount_point)
        .map_err(|e| PoolError::Failed(format!("post-operation pool probe failed: {e}")))?;
    if pool_after.missing_count == 0 && pool_after.devices.len() >= 2 {
        eprintln!("Restoring RAID1 redundancy (soft balance)...");
        pool_balance_raid1_soft(runner, mount_point, progress)?;
        eprintln!("Soft balance complete.");
    }
    Ok(())
}
```

**Direct unit tests for `maybe_restore_raid1()`** (in `pool.rs` `#[cfg(test)]`):

1. **`pre_op_missing_count == 0` → no-op.** Mock returns error for any probe/balance call. Verify function returns Ok without calling anything.
2. **Post-op healthy with ≥2 devices → runs soft balance.** Mock probe returns 0 missing + 2 devices. Assert `BtrfsBalanceRaid1Soft` called.
3. **Post-op still degraded → no balance.** Mock probe returns 1 missing + 2 devices. Assert `BtrfsBalanceRaid1Soft` NOT called.
4. **Post-op healthy with 1 device → no balance.** Mock probe returns 0 missing + 1 device. Assert `BtrfsBalanceRaid1Soft` NOT called.
5. **Post-op probe failure → propagates error.** Mock probe fails. Assert function returns error.

### 3. `cli/src/remove_missing.rs` — add progress param + call shared helper

**3a. Signature change:** add `progress: ProgressOutput` to `cmd_remove_missing`.

**3b. Call `maybe_restore_raid1`:** after disk-map update (line 156), before "Done" message (line 158):
```rust
crate::pool::maybe_restore_raid1(runner, config.mount_point(), pool.missing_count, progress)
    .map_err(|e| RemoveMissingError::Pool(e))?;
```

**3c. Update `compile_steps`:** add `will_clear_last_missing: bool, remaining_present: usize` params. When `will_clear_last_missing && remaining_present >= 2`, append:
```
[long       ] btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)
```

Dry-run gate: `pool.missing_count == 1 && pool.devices.len() >= 2`.

**Tests (RecordingRunner-based, verifying call ordering):**

1. **3-disk pool, 1 missing → soft rebalance runs after remove-missing.** New `RecordingRunner` modeling 3 devices (2 present + 1 missing). Assert ordering: `BtrfsDeviceRemoveMissing` before `BtrfsBalanceRaid1Soft`.
2. **2-disk pool, 1 missing → NO rebalance (1 survivor).** Assert `BtrfsBalanceRaid1Soft` NOT in calls.
3. **3-disk pool, 2 missing, targeting 1 → NO rebalance (still degraded).** New `RecordingRunner` modeling 3 total (1 present + 2 missing). Must also mock the post-operation re-probe to still show 1 missing. Assert `BtrfsBalanceRaid1Soft` NOT in calls.
4. **Dry-run with 1 missing + ≥2 survivors shows rebalance step.**
5. **Dry-run with 1 survivor omits rebalance step.**

### 4. `cli/src/main.rs` — pass `progress` to `cmd_remove_missing`

At the `Commands::RemoveMissing` arm (line 232), resolve progress (matching the pattern used by `Commands::Remove` at line 213) and pass it through.

### 5. `cli/src/replace.rs` — call shared helper for missing path

**5a. Call `maybe_restore_raid1`:** after disk-map update (line 312), before "Done" message (line 314):
```rust
if matches!(&replace_source, ReplaceSource::Missing { .. }) {
    crate::pool::maybe_restore_raid1(runner, config.mount_point(), pool.missing_count, progress)
        .map_err(|e| ReplaceError::Pool(e))?;
}
```

This runs AFTER disk-map is persisted, so a balance failure doesn't leave stale state.

**5b. Update `compile_replace_steps`:** add `will_clear_last_missing: bool, total_devices: u64` params. In the `ReplaceSource::Missing` arm, when `will_clear_last_missing && total_devices >= 2`, append soft balance step. NOT added for `ReplaceSource::Live`.

Dry-run gate: `pool.missing_count == 1 && pool.total_devices >= 2`.

**5c. Fix existing tests:** Update `compile_replace_steps` call sites to pass new params. Update `dry_run_missing_path_shows_btrfs_replace` to assert soft balance step IS present (for `will_clear_last_missing: true`).

**Tests:**
1. **Missing-path dry-run (last missing, ≥2 devices) shows soft rebalance.** (Update existing test.)
2. **Missing-path dry-run (not last missing) omits rebalance.** New test with `will_clear_last_missing: false`.
3. **Live-path dry-run still shows NO balance step.** (Existing test still passes.)
4. **Missing-path dry-run with `total_devices: 1` omits rebalance.** New test.
5. **Missing-path balance failure: disk-map updated before error returned.**

   **Testability seam:** `disk_map.rs` already has `load_disk_map_at(path)` and `save_disk_map_at(path, map)` for testing. Add a matching `update_disk_map_best_effort_at(path, f)` helper (3-line function: load_at → f(&mut map) → save_at, warn on error). Then in `replace.rs`, change the disk-map update call site to use a path that can be overridden in tests. Simplest approach: add an internal `disk_map_path` field or parameter so the test can point at a temp file.

   **Test:** Seed a temp disk-map file with the old disk entry. Use a RecordingRunner where `BtrfsBalanceRaid1Soft` returns exit 1 (failure). Run `cmd_replace` (or a test-internal helper that accepts the disk-map path). Assert: (a) command returns an error, (b) the temp disk-map file reflects the successful replace (new disk recorded, old disk removed) — proving persistence happened before the balance failure.

### 6. `cli/src/doctor.rs` — suggest soft balance (line 341)

Change from `-dconvert=raid1 -mconvert=raid1` to `-dconvert=raid1,soft -mconvert=raid1,soft`.

**Tighten existing tests:** Update `data_profile_mixed_warns` and `metadata_profile_mixed_warns` to assert the exact `,soft` recommendation string (not just `"btrfs balance"` substring):
```rust
assert!(check.message.contains("-dconvert=raid1,soft"), "expected soft flag: {}", check.message);
```

### 7. Doc updates

**`docs/principles.md` line 16:** Update `remove-missing` description. Keep the "cleanup-only" framing but clarify the follow-up balance:

> `remove-missing` cleans up a stale missing-device entry; it never rebuilds data onto a new device (that is `replace`). When clearing the last missing device with ≥2 devices remaining, it runs a follow-up soft balance to restore RAID1 profiles for chunks written during degraded operation.

**`docs/decisions/intent-cli.md` line 21:** Update the command table:

| `braid remove-missing` | Clean up a stale missing-device entry; restores RAID1 profiles if this clears the last missing device | Long-running |
| `braid replace --old <key> --new <key>` | Replace a disk (live or dead) using `btrfs replace start`; restores RAID1 profiles for missing-path when clearing the last missing device | In-place swap (preserves devid) |

**`README.md` lines 152-157:** Update the remove-missing section to note the automatic soft balance.

**`README.md` line 187:** Add note that missing-path replace also restores RAID1 redundancy when clearing the last missing device.

### 8. `tests/repro/degraded-soft-balance.nix` + `.py` — repro proving `,soft` works

New repro test anchoring the feature in observed btrfs behavior. Extends the existing `degraded-writes-single` pattern but uses the `,soft` flag specifically:

1. Create 2-disk LUKS + btrfs RAID1 pool
2. Write baseline data, confirm pure RAID1
3. Kill disk2, mount degraded, write new data
4. Assert single-profile chunks appeared (same as existing repro)
5. Add disk3 as replacement, remove missing
6. Run `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft`
7. Assert all Data+Metadata profiles are back to RAID1 — no single-profile chunks remain

This proves the `,soft` flag is sufficient (not just the non-soft variant proven in existing repro).

## Verification

1. `just test-rust` — all Rust unit tests pass (new + existing)
2. `just test degraded-soft-balance` — new repro passes
3. Manual review: `braid remove-missing --dry-run` shows rebalance step only when clearing last missing with ≥2 survivors
4. Manual review: `braid replace --dry-run` shows rebalance step only for missing-path clearing last missing
5. `just test` — all NixOS VM tests pass

## Files modified

| File | Change |
|------|--------|
| `cli/src/disk_map.rs` | Add `update_disk_map_best_effort_at(path, f)` for testability |
| `cli/src/cmd.rs` | `BtrfsBalanceRaid1Soft` variant + `to_argv()` + test |
| `cli/src/pool.rs` | `pool_balance_raid1_soft()` + `maybe_restore_raid1()` + 5 unit tests |
| `cli/src/remove_missing.rs` | Add `progress` param, call shared helper, update `compile_steps`, 5 tests |
| `cli/src/replace.rs` | Call shared helper after disk-map update, update `compile_replace_steps`, fix + 5 tests |
| `cli/src/main.rs` | Resolve + pass `progress` to `cmd_remove_missing` |
| `cli/src/doctor.rs` | Suggest `,soft` flag + tighten 2 existing tests |
| `docs/principles.md` | Update `remove-missing` description |
| `docs/decisions/intent-cli.md` | Update command table for `remove-missing` and `replace` |
| `README.md` | Update `remove-missing` and `replace` sections |
| `tests/repro/degraded-soft-balance.nix` + `.py` | New repro proving `,soft` restores RAID1 |
