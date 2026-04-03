# Plan: Detect ENOSPC during balance and suggest recovery

## Context

braid's three balance functions (`pool_balance_raid1`, `pool_balance_single`,
`pool_balance_raid1_soft`) report a generic `PoolError::Failed` on any failure.
ENOSPC is a common balance failure mode — btrfs-progs exits 1 and prints
`"No space left on device"` to stderr (via `%m`/`strerror`), with no recovery
guidance. The standard recovery is `btrfs balance start -dusage=0` to free
empty block groups. braid should detect this and tell the user what to do.

The `%m` format specifier expands via `strerror(errno)`, which is
locale-dependent. `RealRunner::exec` does not currently force `LC_ALL=C`, so
on a non-English locale the string would differ. Fixing the locale is a
prerequisite for reliable string-based detection.

## Changes

### 1. Force `LC_ALL=C` in `RealRunner::exec` and `exec_with_stdin` — `cli/src/cmd.rs`

Add `.env("LC_ALL", "C")` to both `std::process::Command` builders (lines 747
and 767). This makes all subprocess output locale-stable, which is correct
since braid already parses stdout/stderr from many tools.

### 2. Add `balance_error` helper — `cli/src/pool.rs`

Extract a shared helper that detects ENOSPC and appends a concrete recovery
hint. No new enum variant — keep `PoolError::Failed` (no caller branches on
balance error type, so a dedicated variant is churn with no benefit).

```rust
fn balance_error(label: &str, mount_point: &str, result: &RawCommandOutput) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("no space left") {
        PoolError::Failed(format!(
            "{label} failed (exit {}): {}\nhint: \
             run `btrfs balance start -dusage=0 {mount_point}` \
             to free empty block groups, then retry",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else {
        PoolError::Failed(format!(
            "{label} failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim(),
        ))
    }
}
```

Add `RawCommandOutput` to the existing `cmd` import line.

### 3. Refactor three balance functions — `cli/src/pool.rs`

Replace inline error construction in `pool_balance_raid1`,
`pool_balance_single`, and `pool_balance_raid1_soft` with calls to
`balance_error`, passing their label and `mount_point`.

### 4. Tests — `cli/src/pool.rs` + `cli/src/remove_missing.rs`

**Helper unit test** (`pool.rs` test module): assert `balance_error` output
contains the hint when stderr has `"No space left on device"`, and does not
contain it otherwise.

**Command-level integration test** (`remove_missing.rs`): the existing
`FailingSoftBalanceRunner` mock already returns ENOSPC stderr (line 994-999).
Add a test (or extend `journal_survives_soft_balance_failure`) that asserts the
final surfaced error string contains `"hint:"` and `"dusage=0"`. This confirms
the hint propagates through `PoolError` → `RemoveMissingError::Pool` → display.

### 5. README troubleshooting note — `README.md`

Add a short entry in the troubleshooting section (or create one if absent) for
balance ENOSPC recovery: what it means, what braid now suggests, and the manual
command.

## Files modified

- `cli/src/cmd.rs` — `LC_ALL=C` in both exec methods
- `cli/src/pool.rs` — `balance_error` helper, refactored callers, unit test
- `cli/src/remove_missing.rs` — integration test asserting hint surfaces
- `README.md` — troubleshooting note

## Verification

1. `just test-rust` — unit + integration tests pass, new tests assert hint text
2. `just test` — VM tests pass (locale change doesn't break existing parsers)
