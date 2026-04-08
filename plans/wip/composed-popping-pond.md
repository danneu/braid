# Plan: Rollback membership on failed `braid replace`

**Status: Superseded** by `cheerful-prancing-hearth.md` — post-commit persist with journal eliminates the need for rollback. Pool.json is never modified before the commit point.

## Context

`braid replace` follows the project's mutation ordering rule (`017-runtime-disk-membership.md:31`):
validate → persist membership → irreversible disk operation → disk-map update.

The pre-commit write at `replace.rs:196-202` ensures that if the persist itself fails, no disk is touched. But if disk operations fail *after* the persist succeeds, `pool.json` is left pointing at the replacement disk while the live pool still depends on the old one. The next `braid unlock` probes the wrong disk set.

**Fix:** Keep the pre-commit write. Add rollback so that membership matches real pool topology at command exit. The commit point is `pool_replace_device()` — the blocking `btrfs replace start -B` call that atomically swaps the device in btrfs metadata.

**Invariant:** membership must match real pool topology at command exit, with `pool_replace_device()` as the replace commit point.

## Rust fix: `cli/src/replace.rs`

### Execution boundary

Everything between `save_membership(&next_membership, ...)` (line 201) and the disk-map update (line 329) is the rollback-eligible zone. Within that zone, the **commit point** is the successful return of `pool_replace_device()` (lines 269/301). This is `btrfs replace start -B` — blocking, atomic. When it returns `Ok`, the new device is the pool member.

### Failure point analysis

| Step | Lines | Pool topology changed? | Rollback safe? |
|---|---|---|---|
| LUKS format | 212 | No | Yes |
| LUKS header backup | 215 | No | Yes |
| LUKS open | 218/228 | No | Yes |
| Keyfile enrollment | 222 | No | Yes |
| `pool_replace_device` (fail) | 269/301 | No | Yes |
| `pool_replace_device` (success) | 269/301 | **Yes — commit point** | **No** |
| `pool_resize_device` | 278/310 | Already changed | No |
| LUKS close old mapper | 281-293 | Already changed | No (already best-effort) |
| `maybe_restore_raid1` | 320-326 | Already changed | No |

### Rollback behavior

On any error after the pre-commit persist but **before** `pool_replace_device()` succeeds: call `save_membership(&current_membership, paths)` exactly once, then return the error. This restores pool.json to match the unchanged pool topology.

On any error **after** `pool_replace_device()` succeeds: do NOT rollback. The pre-committed membership already matches the new pool topology. Return the error as-is.

### Implementation structure

Extract lines 204-327 into a helper `do_replace_disk_ops()` that returns `Result<(), (ReplaceError, bool)>`, where the `bool` is `replace_completed`:

- `Ok(())` — all disk operations succeeded.
- `Err((error, false))` — failed before btrfs replace completed. Pool topology unchanged. **Rollback safe.**
- `Err((error, true))` — failed after btrfs replace completed. Pool topology changed. **Do NOT rollback.**

Inside the helper, a local `let mut replace_completed = false;` is set to `true` immediately after `pool_replace_device` returns `Ok`. All subsequent `?` returns carry `replace_completed = true` through the error tuple.

To thread `replace_completed` through `?`, the helper's body uses an inner closure:

```rust
fn do_replace_disk_ops<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    // ... all needed params
) -> Result<(), (ReplaceError, bool)> {
    let mut replace_completed = false;

    let result: Result<(), ReplaceError> = (|| {
        // LUKS init (lines 204-236)
        // ...?  ← plain ? works, returns ReplaceError

        // btrfs replace (lines 240-313)
        pool_replace_device(runner, devid, &new_mapper_path, mount_point, progress)?;
        replace_completed = true;  // ← commit point

        pool_resize_device(runner, devid, mount_point)?;
        // LUKS close old mapper (best-effort, no ?)
        // maybe_restore_raid1 (missing path only)
        Ok(())
    })();

    result.map_err(|e| (e, replace_completed))
}
```

The caller in `cmd_replace`:

```rust
match do_replace_disk_ops(...) {
    Ok(()) => { /* continue to disk-map update */ }
    Err((e, replace_completed)) => {
        if !replace_completed {
            eprintln!("Replace failed — restoring original pool membership.");
            match membership::save_membership(&current_membership, paths) {
                Ok(()) => return Err(e),
                Err(restore_err) => {
                    eprintln!(
                        "CRITICAL: failed to restore pool membership: {restore_err}\n\
                         Run `braid discover --write` to rebuild pool.json from live pool state."
                    );
                    return Err(ReplaceError::RollbackFailed {
                        original: Box::new(e),
                        rollback: restore_err.to_string(),
                    });
                }
            }
        }
        return Err(e);
    }
}
```

### Error composition

New error variant in `ReplaceError`:

```rust
#[error("replace failed and membership rollback also failed:\n  replace: {original}\n  rollback: {rollback}\nRun `braid discover --write` to rebuild pool.json.")]
RollbackFailed {
    original: Box<ReplaceError>,
    rollback: String,
},
```

Three cases:
1. **Disk ops fail, rollback succeeds:** return the original `ReplaceError`. Pool.json restored. User sees the replace error.
2. **Disk ops fail, rollback fails:** return `ReplaceError::RollbackFailed` containing both errors plus `braid discover --write` recovery instructions.
3. **Disk ops succeed:** no rollback attempt. Return `Ok` or post-commit error as-is (membership already correct).

### What is and is not rolled back

- **`pool.json`** (authoritative membership): rolled back on post-persist failure before the `pool_replace_device()` commit point.
- **`disk-map.json`** (advisory): not touched by rollback. Only updated on the success path (best-effort).
- **LUKS state on new disk**: not rolled back. If LUKS format succeeded before btrfs replace failed, the new disk has a LUKS header but is not in authoritative membership and does not affect unlock correctness.

### Comment update

Replace the comment at line 196:

```rust
// Pre-commit: persist membership swap after all reversible checks pass,
// but before the first irreversible disk operation.
// On failure before pool_replace_device() commits, original membership is restored.
// After pool_replace_device() commits, the pre-committed membership matches pool topology.
```

## Doc updates

### `docs/principles.md:16`

Current text:
> Pre-commit persist: mutating commands write membership to `pool.json` before the irreversible disk operation. If the write fails, the command aborts before touching any disk.

Updated text:
> Pre-commit persist: mutating commands write membership to `pool.json` before the irreversible disk operation. If the write fails, the command aborts before touching any disk. If the disk operation fails before the command's commit point (the point where pool topology actually changes), the original membership is restored so that `pool.json` matches real pool topology at command exit.

### `docs/decisions/017-runtime-disk-membership.md:30-33`

Current text:
> All mutating commands: validate → persist membership to `pool.json` → irreversible disk operation → disk-map update.
>
> Pre-commit writes ensure that if the persist fails, the command aborts before touching any disk.

Updated text:
> All mutating commands: validate → persist membership to `pool.json` → irreversible disk operation → disk-map update.
>
> Pre-commit writes ensure that if the persist fails, the command aborts before touching any disk. If the disk operation fails before the command's commit point — the step where pool topology actually changes (e.g., `pool_replace_device()` for `replace`) — the original membership is restored. After the commit point, the pre-committed membership is authoritative because it already matches the new topology.

### `docs/decisions/012-intent-cli.md` — replace safety constraints section

Add after the existing replace safety constraints (after line 58):

> **Commit-point rollback:** `replace` pre-commits the new membership before disk operations, but restores the original membership if the operation fails before `pool_replace_device()` (the blocking `btrfs replace start`) completes. After `pool_replace_device()` succeeds, the pre-committed membership matches the new pool topology and is not rolled back.

## Rust unit test: post-commit no-rollback

Add a unit test in `replace.rs` `mod tests` that covers the opposite invariant: after `pool_replace_device()` succeeds, a later failure (e.g., `pool_resize_device`) must NOT trigger membership rollback.

### Test: `post_commit_failure_does_not_rollback_membership`

Uses `MockRunner` to simulate:
1. `pool_replace_device` → succeeds (exit 0)
2. `pool_resize_device` → fails (exit 1)

Asserts that after `do_replace_disk_ops` returns `Err((_, replace_completed))`, `replace_completed` is `true`. This directly verifies the commit-point boundary: the caller will see `replace_completed = true` and skip the rollback branch.

This test exercises `do_replace_disk_ops` in isolation — it doesn't need real disks or a pool, just a `MockRunner` with wired outputs for the btrfs commands. The existing test helpers (`two_device_pool`, `mock_with_missing_devids`, `make_replace_config`) provide the scaffolding.

## VM test: `replace-rollback-on-failure`

Uses an undersized replacement disk to trigger a `btrfs replace start` failure after LUKS format succeeds. The repro test `tests/repro/btrfs-replace-rejects-smaller-target` confirms: `btrfs replace start -B -f` rejects a target smaller than the source (512 MiB vs 256 MiB).

### `tests/cli/replace-rollback-on-failure.nix`

- disk1-disk3: 512 MiB (pool members)
- disk4: 128 MiB (replacement — LUKS format succeeds, btrfs replace fails)

### `tests/cli/replace-rollback-on-failure.py`

**Phase 0:** Build 3-disk pool, write test data, save original membership.

**Phase 1:** `braid replace --old disk2 --new disk4` → fails (disk4 too small for btrfs replace).
- Assert pool.json has {disk1, disk2, disk3} — **fails with current code, passes with rollback**
- Assert disk4 NOT in pool.json
- Assert btrfs pool unchanged (disk1, disk2, disk3 present; no missing)
- Assert data readable

**Phase 2:** Lock pool (`braid lock`), unlock with original disks (`braid unlock`), verify data survives round-trip.

Pattern follows `replace-passphrase-mismatch.py`.

### `flake.nix`

Register after `replace-preserves-devid` (~line 231):

```nix
replace-rollback-on-failure = pkgs.testers.nixosTest (
  import ./tests/cli/replace-rollback-on-failure.nix {
    braid = linuxCrane.braid;
  }
);
```

## Files to modify

| File | Change |
|---|---|
| `cli/src/replace.rs` | Add `RollbackFailed` variant; extract `do_replace_disk_ops`; add rollback guard in `cmd_replace` |
| `tests/cli/replace-rollback-on-failure.py` | New VM test |
| `tests/cli/replace-rollback-on-failure.nix` | New VM test config |
| `flake.nix` | Register new test |
| `docs/principles.md` | Add commit-point rollback to pre-commit persist description (line 16) |
| `docs/decisions/017-runtime-disk-membership.md` | Add rollback semantics to mutation ordering section (lines 30-33) |
| `docs/decisions/012-intent-cli.md` | Add commit-point rollback to replace safety constraints (after line 58) |

## Verification

1. Write test + register in flake.nix
2. `just test replace-rollback-on-failure` — fails at "Membership unchanged" (proves the bug)
3. Apply Rust fix
4. `just test replace-rollback-on-failure` — passes
5. `just test replace-live-disk replace-dead-disk replace-passphrase-mismatch` — no regressions
6. `just test-rust` — unit tests pass
7. Review doc changes for consistency across principles.md, 017-runtime-disk-membership.md, and 012-intent-cli.md
