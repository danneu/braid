# Move atomic_write to same-fd fsync via two-layer helpers

## Context

`atomic_write` (state_io.rs:49-70) writes to a temp file, drops the fd, then
`durable_rename` re-opens the file read-only to fsync. This works on Linux
(fsync flushes by inode, not fd) but the conventional pattern is write + fsync
on the same fd. We want `atomic_write` to use that conventional pattern without
weakening `durable_rename`'s safety guarantee for other callers (e.g.
`luks.rs:130` where cryptsetup writes the file externally).

## Design

Two layers:

- **`rename_and_sync_dir`** (new, private) — rename + fsync parent dir. No file
  fsync. Only for callers that have already fsynced their data.
- **`durable_rename`** (unchanged contract) — fsync file + rename + fsync dir.
  Stays the safe default for external writers.

`atomic_write` calls `rename_and_sync_dir` after doing `tmp.sync_all()` on the
write fd. `luks.rs` keeps calling `durable_rename` unchanged.

## Changes

### 1. `cli/src/state_io.rs` — add `rename_and_sync_dir` (private)

```rust
/// Rename a temp file to its final path and fsync the parent directory.
/// Both paths must share the same parent directory.
///
/// Caller must fsync file contents before calling — this only ensures the
/// directory entry is durable.
fn rename_and_sync_dir(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let tmp_dir = tmp.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "tmp path has no parent directory")
    })?;
    let final_dir = final_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "final path has no parent directory")
    })?;
    if tmp_dir != final_dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("rename_and_sync_dir requires same parent directory, got {tmp_dir:?} and {final_dir:?}"),
        ));
    }
    fs::rename(tmp, final_path)?;
    sync_dir(final_dir)
}
```

### 2. `cli/src/state_io.rs` — refactor `durable_rename` to call `rename_and_sync_dir`

```rust
pub fn durable_rename(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let f = File::open(tmp)?;
    f.sync_all()?;
    drop(f);
    rename_and_sync_dir(tmp, final_path)
}
```

`durable_rename` keeps its existing contract (fsync + rename + dir sync). The
same-directory validation moves into `rename_and_sync_dir` so both paths get it.

### 3. `cli/src/state_io.rs` — `atomic_write` uses same-fd fsync

```rust
{
    let mut tmp = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp_path)?;
    tmp.write_all(contents)?;
    tmp.sync_all()?;
}
rename_and_sync_dir(&tmp_path, path)
```

### 4. Tests

Existing `durable_rename` test comments stay accurate — its contract is
unchanged. Add tests for `rename_and_sync_dir` covering observable behavior:

- **rename_and_sync_dir succeeds for same-directory rename** — src removed, dst
  has correct contents.
- **rename_and_sync_dir rejects cross-directory rename** — returns
  `InvalidInput` (same validation as `durable_rename`).

The same-fd `sync_all()` in `atomic_write` is an implementation detail that unit
tests cannot directly prove. Existing `atomic_write` tests already cover the
observable outcome (correct file contents after write+rename).

## Files modified

- `cli/src/state_io.rs` — add `rename_and_sync_dir`, refactor `durable_rename`
  to use it, update `atomic_write` to use same-fd fsync +
  `rename_and_sync_dir`, add tests
- `cli/src/luks.rs` — no changes needed (still calls `durable_rename`)

## Verification

- `just test-rust` — runs existing + new unit tests
