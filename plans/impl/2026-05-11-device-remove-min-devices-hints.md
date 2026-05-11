# plan-the-best-approach-adaptive-dewdrop

## Context

`cli/src/pool.rs` has three kernel-error helpers that wrap `btrfs` mutating
commands: `balance_error` (lines 66-81), `replace_error` (lines 87-102), and
`device_remove_result` (lines 288-297). The first two case-fold stderr,
detect a known kernel rejection, and append a `\nhint: ...` line that tells
the operator the next command to run. The third does not -- it just trims
stderr into a `PoolError::Failed`.

That asymmetry leaks raw kernel strings to the operator on the only
`btrfs` mutation in the trio that can still surprise braid's pre-flight.
Both removal callers (`pool_remove_device` for `braid remove`,
`pool_remove_device_using` for `braid remove-missing`) already gate the
common cases: `RemoveWorkPlan::new` rejects `remaining == 0`
(`remove.rs:111-116`), and `evict_present_device` balances RAID1 -> single
when `remaining == 1` (`pool.rs:371-390`). The remaining failure mode is a
stray non-RAID1 chunk -- braid only authors RAID1 (`cmd.rs:528-552`), but
operator-driven `btrfs balance ... -dconvert=raid1c3` (or future kernel
encoding changes) leaves chunks the kernel will refuse to drop a device
under, surfacing one of:

- `unable to go below two devices on raid1`
- `unable to go below three devices on raid1c3`
- `unable to go below four devices on raid1c4`
- `unable to go below four/two devices on raid10`

(All four come from `BTRFS_ERROR_DEV_RAID*_MIN_NOT_MET`, see
`reference/btrfs-progs/libbtrfs/ioctl.h:901-912` and
`reference/btrfs-progs/common/utils.h:145-150`.)

Three further constraints shape the hint:

1. **The two callers have different recovery paths.**
   `pool_remove_device` is the `braid remove` path (live present-disk
   removal). `pool_remove_device_using` is the `braid remove-missing`
   path (degraded-pool cleanup, called from `remove_missing.rs:237`).
   For a degraded pool, btrfs explicitly forbids "convert to a profile
   with lower redundancy" while a device is missing, instructing the
   operator to "use `btrfs replace` or `btrfs device remove` to handle
   the failing/missing device first" (`reference/btrfs-progs/Documentation/btrfs-balance.rst:240-254`).
   braid's own docs reinforce this: `remove-missing` does not rebuild
   data, "use `braid replace` for that"
   (`manual/commands/remove-missing.md:5`). A "rebalance back to RAID1"
   hint is correct for the live path but wrong for the missing path.

2. **The 2-disk RAID1 + 1 missing case is already pre-flighted.**
   `plan_remove_missing` rejects that exact topology at
   `remove_missing.rs:398-425`, documented at
   `manual/commands/remove-missing.md:83`. So the iconic
   `unable to go below two devices on raid1` stderr is no longer
   CLI-reachable from `pool_remove_device_using`; the realistic
   Missing-path fallthrough is a non-RAID1 chunk (`raid1c3`/`raid1c4`/
   `raid10`) requiring more devices than would remain in a 3+ disk
   pool. The Missing wrapper test uses `raid1c3` stderr to reflect
   this. The helper-level tests still cover the `raid1` variant
   under the Live context because `evict_present_device`'s
   balance-to-single path could in principle be bypassed by a future
   change, but the Live wrapper test also targets the realistic
   leftover-chunk case for symmetry.

3. **The journal is already on disk when the failure surfaces.**
   Both callers `journal::write_journal` *before* the btrfs op
   (`remove.rs:249`, `remove_missing.rs:224`). After the failed device
   remove, `pending-op.json` remains, and
   `preflight::check_no_pending_operation` (`preflight.rs:42-55`)
   blocks every mutator except `recover`, `status`, and `lock`. Any
   hint that says "then retry" without first telling the operator to
   run `braid recover` sets up a second failure in the recovery-mode
   gate. Verified recover semantics for both paths: for `OpKind::Remove`,
   `execute_generic_live_pool_recovery` (`recover.rs:951-1017`)
   reconciles pool.json from the live pool and clears the journal; for
   `RemoveMissing { phase: PoolMutation, ... }` with the device still
   missing, `execute_remove_missing_pool_mutation_recovery`
   (`recover.rs:2325-2381`) restores pre-membership, clears the
   journal, and prints "Re-run braid remove-missing to retry" -- both
   reset cleanly so the next mutator can run.

The originating finding proposed pointing the operator at `braid replace
--missing-id <id>`, but that flag is gated to dead disks at
`replace.rs:1272-1280`; it cannot unblock a present-disk remove. The
correct hint distinguishes the two callers and routes through `braid
recover` so the followup command actually runs.

## Approach

Factor a `device_remove_error` helper that mirrors the shape of
`balance_error` and `replace_error`, but parameterize it on a
`RemoveContext` enum so the two recovery sequences are spelled
correctly. `device_remove_result` becomes a thin router:
exit-status check, dispatch to the helper. Each caller passes its own
hardcoded context (Live for `pool_remove_device`, Missing for
`pool_remove_device_using`); no plumbing through public function
signatures beyond the context arg.

Detect the kernel's `unable to go below` substring (covers all four
RAID variants in one branch) and emit a context-specific hint; fall
through to a plain error for unrelated stderr. ENOSPC is intentionally
not handled here -- `preflight::check_eviction_space` at `remove.rs:404`
already gates it, and the right `device remove` ENOSPC recovery ("add a
disk or free user data") differs from `balance_error`'s `dusage=0` path,
so duplicating the arm would be shallow symmetry.

### Files to modify

- `cli/src/pool.rs` -- only file touched. Add `RemoveContext` enum and
  `device_remove_error`, reshape `device_remove_result` to take the
  context plus `mount_point`, update the two call sites
  (`pool_remove_device` line 259, `pool_remove_device_using` line 285),
  and add unit tests in the existing `mod tests`.

### Helper to add

Place between `replace_error` (line 102) and `pool_balance_raid1`
(line 105) so the three error helpers stay grouped:

```rust
/// Recovery-context for a failed `btrfs device remove`. `Live` means
/// the operator was removing a present disk via `braid remove`; the
/// hint can suggest rebalancing back to RAID1 because the pool is
/// not degraded. `Missing` means the operator was clearing a missing
/// slot via `braid remove-missing`; the hint must steer toward
/// repairing the missing device, because btrfs forbids lowering
/// redundancy while a device is missing
/// (`reference/btrfs-progs/Documentation/btrfs-balance.rst:240-254`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveContext {
    Live,
    Missing,
}

/// Build a `PoolError::Failed` for `btrfs device remove`, decoding the
/// kernel's `BTRFS_ERROR_DEV_RAID*_MIN_NOT_MET` family ("unable to go
/// below ...") and appending a recovery hint. The hint splits on
/// `RemoveContext`: Live recovers via raw `btrfs balance` + `braid
/// recover`; Missing recovers via `braid recover` + `braid replace
/// --missing-id`. `braid recover` must come before any braid mutator
/// because both callers wrote `pending-op.json` before the failed
/// btrfs op (`remove.rs:249`, `remove_missing.rs:224`), and
/// `check_no_pending_operation` (`preflight.rs:42-55`) blocks every
/// mutator until the journal is cleared.
fn device_remove_error(
    ctx: RemoveContext,
    mount_point: &MountPoint,
    result: &RawCommandOutput,
) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("unable to go below") {
        let hint = match ctx {
            RemoveContext::Live => format!(
                "a non-RAID1 chunk likely requires more devices than will remain. \
                 Inspect with `btrfs filesystem usage {mount_point}`, then \
                 `btrfs balance start -dconvert=raid1 -mconvert=raid1 -f {mount_point}` \
                 to convert it back to RAID1, then `braid recover` to clear the \
                 pending operation, then retry `braid remove`."
            ),
            RemoveContext::Missing =>
                "a non-RAID1 chunk requires more devices than will remain. \
                 While a device is missing, do not lower redundancy -- \
                 repair the missing device instead. Run `braid recover` to \
                 clear the pending operation, then `braid replace --missing-id <devid>` \
                 to rebuild data onto a replacement disk."
                .to_owned(),
        };
        PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}\nhint: {hint}",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else {
        PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim(),
        ))
    }
}
```

### `device_remove_result` reshape

Change signature to take `RemoveContext`, `&MountPoint`, and
`&RawCommandOutput` (matching sibling helpers; the previous `result:
RawCommandOutput` consumed value is not needed by either caller):

```rust
fn device_remove_result(
    ctx: RemoveContext,
    mount_point: &MountPoint,
    result: &RawCommandOutput,
) -> Result<(), PoolError> {
    if result.exit_status != 0 {
        return Err(device_remove_error(ctx, mount_point, result));
    }
    Ok(())
}
```

Update the two call sites:

- `pool.rs:259` (`pool_remove_device`):
  `device_remove_result(result)` -> `device_remove_result(RemoveContext::Live, mount_point, &result)`.
- `pool.rs:285` (`pool_remove_device_using`):
  `device_remove_result(result)` -> `device_remove_result(RemoveContext::Missing, mount_point, &result)`.

Both functions already have `mount_point` in scope. The `RemoveContext`
value at each call site is fixed by which public function we're in --
`pool_remove_device` only services `braid remove` via
`evict_present_device` (live), and `pool_remove_device_using` only
services `braid remove-missing` via `remove_missing.rs:237` (missing) --
so no caller plumbing is required.

### Tests

Add seven unit tests inside `mod tests`, alongside the existing
`balance_error_*` tests (around `pool.rs:1092-1135`). Tests 1-5
exercise `device_remove_result` directly so a regression in the
substring-match or context dispatch is pinpointed at the helper layer.
Tests 6-7 go through the public wrappers `pool_remove_device` and
`pool_remove_device_using`, locking in the call-site context choice --
the highest-risk part of the change, since the compiler enforces
presence of the `RemoveContext` arg but not its value (a swapped
`Live`/`Missing` would silently route to the wrong hint). Mirror the
intent/why/scenario preamble convention. Reuse `mp()` and the
`RawCommandOutput` literal pattern from the existing tests.

Variant choice: the Missing-path tests (#3, #7) use `raid1c3` stderr
because the iconic `raid1` two-device case is now pre-flighted by
`plan_remove_missing` at `remove_missing.rs:398-425` and is no longer
CLI-reachable from `pool_remove_device_using`. The realistic Missing
failure is a stray non-RAID1 chunk requiring more devices than would
remain in a 3+ disk pool. Live-path tests keep the iconic `raid1`
stderr in #1 and #6 as a defense-in-depth assertion (no
`evict_present_device` regression silently re-introduces the case)
and add `raid1c3` coverage in #2 to lock in the broad-substring
match.

1. **`device_remove_result_live_raid1_min_includes_balance_hint`** --
   positive, Live: stderr `"ERROR: error removing device
   '/dev/mapper/braid-disk2': unable to go below two devices on raid1"`
   yields an `Err` whose message contains `hint:`,
   `dconvert=raid1`, `braid recover`, `braid remove`, and `/mnt/storage`.

2. **`device_remove_result_live_raid1c3_min_includes_balance_hint`** --
   positive, Live: stderr containing `unable to go below three devices
   on raid1c3` produces the same Live hint. Locks in the
   broad-substring design across kernel variants.

3. **`device_remove_result_missing_raid1c3_min_includes_replace_hint`**
   -- positive, Missing: stderr containing `unable to go below three
   devices on raid1c3` with `RemoveContext::Missing` yields an `Err`
   whose message contains `hint:`, `braid replace --missing-id`, and
   `braid recover`, and does NOT contain `dconvert=raid1` or `btrfs
   balance` (proves the two contexts route to different hints, on the
   CLI-reachable Missing-path variant).

4. **`device_remove_result_no_hint_for_unrelated_failure`** -- negative:
   stderr `"ERROR: device is busy"` (in either context) produces an
   `Err` whose message does NOT contain `hint:`, mirroring
   `pool_replace_device_no_hint_for_unrelated_failure` at
   `pool.rs:1254`.

5. **`device_remove_result_ok_passes_through`** -- success path:
   `exit_status == 0` returns `Ok(())` regardless of context. Locks in
   the routing function's no-op behavior on success and prevents the
   helper from being accidentally called on success in a future
   refactor.

6. **`pool_remove_device_failure_emits_live_balance_hint`** -- wrapper,
   call-site context lock-in: invokes `pool_remove_device(&runner,
   "/dev/mapper/braid-disk2", &mp(), ProgressOutput::Off)` against a
   `MockRunner` whose `BtrfsDeviceRemove` returns the RAID1 min-devices
   stderr with `exit_status: 1`. Asserts the resulting `Err` contains
   the Live hint markers (`dconvert=raid1`, `braid recover`, `braid
   remove`) and does NOT contain the Missing markers (`braid replace
   --missing-id`). `ProgressOutput::Off` short-circuits the heartbeat
   thread (`progress.rs:299-301`), so a plain `MockRunner` is
   sufficient -- no extra threading or sink machinery.

7. **`pool_remove_device_using_failure_emits_missing_replace_hint`** --
   wrapper, call-site context lock-in: same shape as #6 but invokes
   `pool_remove_device_using(&runner, "2", &mp(), ProgressOutput::Off,
   &FakeSleeper::default(), &RecordingSink::default())` (the
   `FakeSleeper` and `RecordingSink` types already exist at
   `pool.rs:761-829`). Stderr is `raid1c3` -- the CLI-reachable
   Missing-path variant after the `remove_missing.rs:398-425`
   pre-flight rules out the `raid1` two-device case. Asserts the
   resulting `Err` contains the Missing markers (`braid replace
   --missing-id`, `braid recover`) and does NOT contain the Live
   markers (`dconvert=raid1`, `btrfs balance`).

## Verification

End-to-end checks before declaring done:

1. `just test-rust` -- runs `cargo test`. The seven new tests must
   pass; none of the existing pool tests should regress. The two
   wrapper-level tests (#6, #7) automatically catch a swapped
   `Live`/`Missing` context at either call site, so the routing
   correctness is enforced by the test suite, not by manual review.
2. `cargo build` (or `nix build` of the CLI) -- ensures both call-site
   signature updates compile. The two `device_remove_result(result)`
   sites are the only ones in tree (`grep -rn "device_remove_result"
   cli/src/`); a missed call site would surface as a compile error.
3. Sanity: `grep -rn "unable to go below" cli/src/` should return only
   the new helper plus its tests after the change. No prior occurrences
   exist (`git log -S 'go below'` shows zero hits in the CLI source).

No VM test needed: the kernel rejection path is unit-testable from
`device_remove_result` directly, and the routing under test does not
touch the runner or filesystem. The existing `balance_error_*` and
`replace_error` tests are the canonical precedent.

## Out of scope

- ENOSPC arm (already gated by `preflight::check_eviction_space`;
  add only if a real bypass is observed).
- Decoding for `pool_add_device`, `pool_resize_device`, or
  `pool_bootstrap_mount[_raid1]` -- those operate on different `btrfs`
  subcommands whose failure modes don't share the min-devices class.
- Changing pre-flight: this plan adds a fallthrough hint, not a new gate.
- Plumbing the missing devid into the Missing hint. The helper does
  not have it (`pool_remove_device_using` takes a stringified devid
  but the helper signature stays small and matches its siblings); the
  hint references `<devid>` as a placeholder and the operator already
  knows the value from their own `--missing-id` argument.
