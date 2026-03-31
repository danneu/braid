# Fix: lock.rs — close mappers even when umount fails

## Context

`cmd_lock` (lines 81-89) returns immediately when `umount` fails, skipping all LUKS mapper cleanup. Since `lock` is the `ExecStop` for `braid-online.service`, a stuck umount during shutdown leaves LUKS mappers open — encrypted data remains accessible, no cleanup attempted.

## Approach

When umount fails: record the error, continue to mapper cleanup, return the umount error at the end. Only **busy/in-use** mapper close failures are suppressed after a failed umount — unexpected errors (permissions, missing device, etc.) remain fatal. `btrfs device scan --forget` stays gated on successful unmount.

## Changes — `cli/src/lock.rs`

### 1. Add `LockError::DeviceBusy` variant (line ~15)

```rust
#[error("device busy: {0}")]
DeviceBusy(String),
```

Distinguishes busy-after-retries from unexpected failures. Only `close_mapper_with_retry` produces this variant.

### 2. Update `close_mapper_with_retry` to return `DeviceBusy` (lines 30-54)

Split the final-attempt error path: if `is_busy`, return `DeviceBusy`; otherwise return `Failed`.

```rust
if !is_busy {
    return Err(LockError::Failed(format!(...)));
}
if attempt == CLOSE_RETRY_ATTEMPTS {
    return Err(LockError::DeviceBusy(format!(...)));
}
```

### 3. Defer umount error instead of returning (lines 77-90)

```rust
let mut umount_error: Option<LockError> = None;
```

On umount failure: store into `umount_error`, print warning that mapper close will be attempted. Do **not** proceed to `btrfs device scan --forget` (keep it gated on successful unmount).

```rust
if umount_result.exit_status != 0 {
    let err = LockError::Failed(format!(...));  // same message as today
    eprintln!("[FAIL]  {err}");
    eprintln!("[warn]  attempting to close LUKS mappers despite umount failure...");
    umount_error = Some(err);
} else {
    eprintln!("{}  {:<14}unmounted {}", tag("ok"), "pool", mount_point);

    // btrfs device scan --forget (only after successful unmount)
    ...
}
```

### 4. Wrap mapper close with precise error handling (lines 111-124)

```rust
match close_mapper_with_retry(runner, &mn.0) {
    Ok(()) => { eprintln!("...locked"); }
    Err(LockError::DeviceBusy(msg)) if umount_error.is_some() => {
        // Expected: filesystem still holds device busy
        eprintln!("[warn]  disk: {:<7}close failed (umount was stuck): {}", name, msg);
    }
    Err(e) => return Err(e),  // Unexpected error — fatal even after umount failure
}
```

### 5. Same precise handling for orphan mapper close (lines 126-149)

Identical `match` pattern — only `DeviceBusy` is suppressed when `umount_error.is_some()`.

### 6. Return deferred umount error at end (before "already locked")

```rust
if let Some(err) = umount_error {
    return Err(err);
}
```

### 7. Tests

**Update** `lock_umount_busy_fails` and `lock_umount_busy_includes_hint`:
- Add `CryptsetupClose` mocks with `"Device is still in use."` stderr (triggers `DeviceBusy`)
- Add required intent/why/scenario block comments to both (they currently lack them)

**New tests** (all with required intent/why/scenario block comments):

- `lock_umount_fails_but_mappers_close_successfully` — umount fails, mapper closes succeed, umount error still returned. Proves mappers are attempted.
- `lock_umount_fails_busy_mapper_is_warning` — umount fails, mapper close returns busy, suppressed as warning, umount error returned.
- `lock_umount_fails_unexpected_mapper_error_is_fatal` — umount fails, mapper close returns non-busy error ("Device is not active."), that error is returned (not the umount error). Key guard for precise suppression.
- `lock_mapper_close_fatal_when_umount_succeeded` — regression guard: mapper close errors remain fatal on normal path.
- `lock_umount_fails_orphan_busy_is_warning` — umount fails, orphan mapper close returns busy, suppressed as warning. Covers the orphan branch separately from the membership branch.
- `lock_umount_fails_orphan_unexpected_error_is_fatal` — umount fails, orphan mapper close returns non-busy error, fatal. Proves the orphan branch also only suppresses `DeviceBusy`.

## Files modified

- `cli/src/lock.rs` — all changes in this one file

## Verification

```
just test-rust
```
