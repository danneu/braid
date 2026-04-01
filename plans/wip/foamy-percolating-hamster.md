# Make `clear_journal` durable

## Context

`clear_journal` (`cli/src/journal.rs:74-79`) uses a bare `std::fs::remove_file()` without fsyncing the parent directory. If the system crashes before the directory metadata is flushed, the journal file can reappear on reboot, triggering unnecessary recovery mode. The rest of the I/O in braid (`atomic_write`, `durable_rename` in `state_io.rs`) correctly fsyncs the directory — `clear_journal` is the one gap.

## Plan

### 1. Add `sync_dir` helper to `state_io.rs`

The dir-fsync pattern already exists inline in `durable_rename` (`state_io.rs:38-40`). Extract it into a public `sync_dir(path: &Path)` function so both `durable_rename` and `clear_journal` can use it without duplication.

```rust
/// Fsync a directory to flush metadata (renames, deletions) to disk.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}
```

Then update `durable_rename` to call `sync_dir(final_dir)?` instead of its inline equivalent.

### 2. Make `clear_journal` durable in `journal.rs`

After `remove_file` succeeds, require a valid parent directory (matching the style in `durable_rename` at `state_io.rs:9`) and fsync it. Fail with `JournalError::Delete` if the parent is missing — never silently skip the durability step.

```rust
pub fn clear_journal(paths: &StatePaths) -> Result<(), JournalError> {
    let path = paths.pending_op_json();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let dir = path.parent().ok_or_else(|| {
                JournalError::Delete(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "journal path has no parent directory",
                ))
            })?;
            crate::state_io::sync_dir(dir).map_err(JournalError::Delete)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(JournalError::Delete(e)),
    }
}
```

### 3. Add targeted unit tests

**In `state_io.rs` tests** — test `sync_dir` directly:
- `sync_dir_on_valid_directory`: call `sync_dir` on a tempdir, assert `Ok`.
- `sync_dir_on_nonexistent_directory`: call `sync_dir` on a path that doesn't exist, assert `Err`.

**In `journal.rs` tests** — test that `clear_journal` performs the durable delete:
- `clear_journal_fsyncs_directory`: write a journal, clear it, verify file is gone and `Ok` returned. (Confirms the new code path executes without error — true crash persistence can't be tested in unit tests, but this validates the fsync call doesn't fail.)

### Files to modify

- `cli/src/state_io.rs` — add `sync_dir`, refactor `durable_rename` to use it, add `sync_dir` tests
- `cli/src/journal.rs` — update `clear_journal` to require parent + fsync, add durable-delete test

### Verification

- `just test-rust` — all existing tests pass, new tests pass.
