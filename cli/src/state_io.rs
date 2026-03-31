use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Rename a temp file to its final path and fsync the parent directory.
/// Both paths must share the same parent directory.
///
/// Caller must fsync file contents before calling — this only ensures the
/// directory entry is durable.
fn rename_and_sync_dir(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let tmp_dir = tmp.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "tmp path has no parent directory",
        )
    })?;
    let final_dir = final_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "final path has no parent directory",
        )
    })?;
    if tmp_dir != final_dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "rename_and_sync_dir requires same parent directory, got {tmp_dir:?} and {final_dir:?}"
            ),
        ));
    }
    fs::rename(tmp, final_path)?;
    sync_dir(final_dir)
}

/// Durably rename an already-written temp file to its final path.
/// Both paths must share the same parent directory (same-directory rename).
/// Fsyncs the temp file, renames, then fsyncs the parent directory.
pub fn durable_rename(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let f = File::open(tmp)?;
    f.sync_all()?;
    drop(f);
    rename_and_sync_dir(tmp, final_path)
}

/// Fsync a directory to flush metadata (renames, deletions) to disk.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}

/// Atomically replace a file by writing to a temp file in the same directory,
/// fsyncing data, renaming, then fsyncing the parent directory.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp_path = dir.join(format!(".{}.tmp", file_name.to_string_lossy()));

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("state.json");
        atomic_write(&path, br#"{"ok":true}"#).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("\"ok\""));
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        atomic_write(&path, b"v1").unwrap();
        atomic_write(&path, b"v2").unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        assert_eq!(contents, "v2");
    }

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
    #[test]
    fn durable_rename_syncs_and_renames() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data.tmp");
        let dst = tmp.path().join("data.final");
        fs::write(&src, b"header-bytes").unwrap();
        durable_rename(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"header-bytes");
    }

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
    #[test]
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

    /*
     * Intent: sync_dir succeeds on a valid directory.
     *
     * Why it exists: sync_dir is the shared primitive for durable renames
     * and durable deletions. A failure here would break both code paths.
     *
     * Scenario: after writing or deleting a file, the caller fsyncs the
     * parent directory to ensure the metadata change survives power loss.
     */
    #[test]
    fn sync_dir_on_valid_directory() {
        let tmp = TempDir::new().unwrap();
        sync_dir(tmp.path()).unwrap();
    }

    /*
     * Intent: rename_and_sync_dir moves a temp file to its final path
     * within the same directory.
     *
     * Why it exists: rename_and_sync_dir is the lower-level primitive
     * used by atomic_write after same-fd fsync. It must correctly rename
     * and remove the source.
     *
     * Scenario: atomic_write creates a temp file, fsyncs it, then calls
     * rename_and_sync_dir to atomically place the final file.
     */
    #[test]
    fn rename_and_sync_dir_same_directory() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data.tmp");
        let dst = tmp.path().join("data.final");
        fs::write(&src, b"payload").unwrap();
        rename_and_sync_dir(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
    }

    /*
     * Intent: rename_and_sync_dir rejects cross-directory renames.
     *
     * Why it exists: same-directory rename + single dir fsync is the
     * durability contract. Cross-directory paths would silently weaken it.
     *
     * Scenario: a caller accidentally passes paths in different directories —
     * this must fail loudly.
     */
    #[test]
    fn rename_and_sync_dir_rejects_cross_directory() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        let src = dir_a.join("data.tmp");
        let dst = dir_b.join("data.final");
        fs::write(&src, b"bytes").unwrap();
        let err = rename_and_sync_dir(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /*
     * Intent: sync_dir fails on a nonexistent directory.
     *
     * Why it exists: callers rely on sync_dir returning an error when the
     * directory is invalid, rather than silently succeeding.
     *
     * Scenario: a bug passes a bogus path to sync_dir — the error must
     * propagate so the caller can report the durability failure.
     */
    #[test]
    fn sync_dir_on_nonexistent_directory() {
        let tmp = TempDir::new().unwrap();
        let bogus = tmp.path().join("does-not-exist");
        let err = sync_dir(&bogus).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
