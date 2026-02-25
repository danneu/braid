use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Atomically replace a file by writing to a temp file in the same directory,
/// fsyncing data, renaming, then fsyncing the parent directory.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(dir)?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no file name")
    })?;
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

    fs::rename(&tmp_path, path)?;

    // Sync directory metadata so rename survives power loss.
    let dir_fd = File::open(dir)?;
    dir_fd.sync_all()?;
    Ok(())
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
}
