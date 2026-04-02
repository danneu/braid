# Fix crash-durability gap in LUKS header backup

## Context

`backup_luks_header_to()` (`cli/src/luks.rs:99`) writes a LUKS header backup via temp-file + rename, but is missing two fsyncs: one on the temp file before rename, and one on the parent directory after rename. A crash at the wrong moment could leave a zero-length or corrupt `.luksheader` file — defeating the purpose of the backup.

`atomic_write()` in `cli/src/state_io.rs:7` already handles the full durable sequence (write → fsync file → rename → fsync dir), but it takes `&[u8]` contents. `backup_luks_header_to` can't use it directly because cryptsetup writes the temp file externally. Having two subtly-different atomic-write paths is how this bug appeared in the first place.

## Plan

### 1. Add `durable_rename()` to `cli/src/state_io.rs`

Extract a new helper that handles the "finalize an already-written temp file" case:

```rust
/// Durably rename an already-written temp file to its final path.
/// Both paths must share the same parent directory (same-directory rename).
/// Fsyncs the temp file, renames, then fsyncs the parent directory.
pub fn durable_rename(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let tmp_dir = tmp.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "tmp path has no parent directory")
    })?;
    let final_dir = final_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "final path has no parent directory")
    })?;
    if tmp_dir != final_dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "durable_rename requires same parent directory, got {tmp_dir:?} and {final_dir:?}"
            ),
        ));
    }

    // Flush temp file contents to disk.
    let f = File::open(tmp)?;
    f.sync_all()?;
    drop(f);

    fs::rename(tmp, final_path)?;

    // Sync directory metadata so rename survives power loss.
    let dir_fd = File::open(final_dir)?;
    dir_fd.sync_all()?;
    Ok(())
}
```

### 2. Rewrite `atomic_write()` to use `durable_rename()`

Reduces duplication — `atomic_write` writes the temp file then delegates to `durable_rename`:

```rust
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| { ... })?;
    fs::create_dir_all(dir)?;

    let file_name = path.file_name().ok_or_else(|| { ... })?;
    let tmp_path = dir.join(format!(".{}.tmp", file_name.to_string_lossy()));

    {
        let mut tmp = OpenOptions::new()
            .create(true).truncate(true).write(true)
            .open(&tmp_path)?;
        tmp.write_all(contents)?;
        // fsync happens inside durable_rename, skip it here
    }

    durable_rename(&tmp_path, path)
}
```

### 3. Use `durable_rename()` in `backup_luks_header_to()` (`cli/src/luks.rs:130`)

Replace the bare `fs::rename` call with:

```rust
crate::state_io::durable_rename(&tmp_path, &backup_path)?;
```

### 4. Add unit test in `cli/src/state_io.rs`

```rust
#[test]
/*
 * Intent: durable_rename fsyncs the temp file, renames it to the final
 * path, and fsyncs the parent directory.
 *
 * Why it exists: backup_luks_header_to previously skipped both fsyncs,
 * risking corrupt or missing LUKS header backups after power loss.
 *
 * Scenario: NAS loses power right after cryptsetup writes a LUKS header
 * backup to a temp file. Without fsync before rename + dir fsync after,
 * the renamed file could be zero-length or the directory entry could be
 * lost entirely.
 */
fn durable_rename_syncs_and_renames() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("data.tmp");
    let dst = tmp.path().join("data.final");
    fs::write(&src, b"header-bytes").unwrap();
    durable_rename(&src, &dst).unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read(&dst).unwrap(), b"header-bytes");
}

#[test]
/*
 * Intent: durable_rename rejects cross-directory renames.
 *
 * Why it exists: the durability guarantee only holds for same-directory
 * renames (single dir fsync). Allowing cross-directory paths would
 * silently weaken crash safety.
 *
 * Scenario: a future caller accidentally passes paths in different
 * directories — this must fail loudly rather than produce a false sense
 * of durability.
 */
fn durable_rename_rejects_cross_directory() {
    let tmp = TempDir::new().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let src = dir_a.join("data.tmp");
    let dst = dir_b.join("data.final");
    fs::write(&src, b"bytes").unwrap();
    let err = durable_rename(&src, &dst).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}
```

Note: we can't directly assert that fsync was called without a mock filesystem layer, but we _can_ verify the rename semantics and the same-directory guard. The fsync ordering is enforced structurally by `durable_rename`'s implementation — the test locks down that all callers go through this single code path.

## Files modified

- `cli/src/state_io.rs` — add `durable_rename()`, refactor `atomic_write()` to use it, add test
- `cli/src/luks.rs:130` — replace `fs::rename` with `durable_rename()`

## Verification

1. `just test-rust` — existing + new unit tests pass
2. `just test luks-header-backup` — NixOS VM test still passes
