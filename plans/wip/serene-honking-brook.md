# Fix: early return on mapper close error skips remaining mappers

## Context

`cmd_lock` is the shutdown path (ExecStop of `braid-online.service`): unmount btrfs, close all LUKS mappers, clean up orphans. When a mapper close fails with a non-busy error, the function returns immediately at two sites (line 143 and 175), skipping all remaining mappers. This leaves LUKS devices open after the system thinks shutdown is complete.

The fix: accumulate errors instead of early-returning, attempt all mappers, then return `first_mapper_error.or(umount_error)`. This preserves existing error precedence — non-busy mapper errors remain primary even when umount also failed.

## File to modify

`cli/src/lock.rs` — single file change.

## Implementation

### 1. Add error accumulator

After the `umount_error` declaration (~line 87), add:

```rust
let mut first_mapper_error: Option<LockError> = None;
```

### 2. Replace early returns with error accumulation

**Loop 1 — membership mappers (line 143).** Change:
```rust
Err(e) => return Err(e),
```
to:
```rust
Err(e) => {
    eprintln!("[FAIL]  disk: {:<7}{}", name, e);
    if first_mapper_error.is_none() {
        first_mapper_error = Some(e);
    }
}
```

**Loop 2 — orphan mappers (line 175).** Same change:
```rust
Err(e) => {
    eprintln!("[FAIL]  disk: {:<7}orphan: {}", disk_name, e);
    if first_mapper_error.is_none() {
        first_mapper_error = Some(e);
    }
}
```

The `DeviceBusy` suppression arms (lines 137-142, 169-174) are unchanged.

### 3. Replace error-return logic at end of function (lines 186-196)

```rust
// Return first fatal mapper error if any, otherwise deferred umount error
if let Some(e) = first_mapper_error {
    return Err(e);
}
if let Some(e) = umount_error {
    return Err(e);
}

if !pool_was_mounted && all_already_closed {
    eprintln!("pool already locked");
}

Ok(())
```

This is `first_mapper_error.or(umount_error)` — preserves current precedence exactly. Non-busy mapper errors are still the primary returned error even when umount also failed. The only behavioral change is that remaining mappers are now attempted before the error is returned.

## Test changes

### Recording runner for the regression test

`MockRunner` does not record calls and does not fail on unused mocks, so we cannot use it to prove that all mappers were attempted. Add a thin `RecordingRunner` in the `lock::tests` module:

```rust
/// A runner that delegates to MockRunner but records which
/// CryptsetupClose requests were made.
struct RecordingRunner {
    inner: MockRunner,
    close_calls: std::cell::RefCell<Vec<String>>,
}

impl RecordingRunner {
    fn new(inner: MockRunner) -> Self {
        Self {
            inner,
            close_calls: RefCell::new(Vec::new()),
        }
    }

    fn close_calls(&self) -> Vec<String> {
        self.close_calls.borrow().clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        if let CmdRequest::CryptsetupClose { mapper } = request {
            self.close_calls.borrow_mut().push(mapper.clone());
        }
        self.inner.run(request)
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        self.inner.run_with_stdin(request, stdin)
    }
}
```

### Update 2 existing tests (add missing mocks)

These tests only mock the first failing mapper. With the fix, remaining mappers are attempted and hit `MissingMock`:

1. **`lock_umount_fails_unexpected_mapper_error_is_fatal`** (line 772) — add `braid-bbb` ok mock. Assertion unchanged: the returned error is still the non-busy mapper error (first_mapper_error takes precedence over umount_error).

2. **`lock_mapper_close_fatal_when_umount_succeeded`** (line 798) — add `braid-bbb` ok mock. Assertion unchanged.

### Update 1 existing test doc comment

3. **`lock_umount_fails_orphan_unexpected_error_is_fatal`** (line 875) — all mocks already present. The returned error is still the orphan's non-busy error (first_mapper_error precedence). Update doc comment to note that remaining mappers are still attempted before the error is returned.

### Add 2 new tests

4. **`lock_continues_closing_after_mapper_error`** — uses `RecordingRunner`. aaa fails (non-busy), bbb succeeds. Assert both `braid-aaa` and `braid-bbb` appear in `close_calls()`. This is the primary regression guard.

5. **`lock_collects_first_mapper_error`** — both aaa and bbb fail (non-busy). The returned error mentions `braid-aaa` (the first). Uses `RecordingRunner` to verify both were attempted.

### Unaffected tests (13)

All other existing tests either: have complete mocks for all mappers, only involve `DeviceBusy` (suppressed, not captured), or test the unmounted/already-locked path.

## Verification

```sh
just test-rust
```

All 18 tests in `lock.rs` should pass (16 existing with adjustments + 2 new).
