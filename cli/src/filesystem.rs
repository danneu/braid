/// The crate's single filesystem seam: four reads (`exists`,
/// `is_block_device`, `list_dir`, `read_to_string`) plus the mount/pool
/// execute layer's one mutation (`create_dir_all`).
///
/// Every direct `std::fs` syscall braid makes goes through here rather than
/// through a subprocess (cf. ADR 016), so probe, mount, idle and preflight
/// paths stay unit-testable against in-memory doubles instead of a real disk.
pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
    fn is_block_device(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error>;
    /// Create `path` and any missing parents -- the mount/pool execute layer's
    /// one filesystem mutation, kept behind this seam (a direct `std::fs`
    /// syscall, not a subprocess; cf. ADR 016) so the mount path is mockable
    /// and the failure surfaces fail-closed. No default: every impl declares
    /// its behavior, and read-only doubles `unreachable!` here so an accidental
    /// mutation on a read-only path fails loudly instead of passing silently.
    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error>;
}

/// The one production `Filesystem`: unmocked `std::fs` against the live host.
/// Constructed at command entry points so everything below them takes the seam
/// by reference and can be driven by a double under test.
pub struct RealFilesystem;

impl Filesystem for RealFilesystem {
    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn is_block_device(&self, path: &str) -> bool {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path)
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false)
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        std::fs::read_to_string(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    names.push(entry?.file_name().to_string_lossy().into_owned());
                }
                Ok(names)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(path)
    }
}
