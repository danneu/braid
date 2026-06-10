//! Shared `/dev/disk/by-id/` symlink handling.
//!
//! The prefix-priority and partition-filtering helpers (`by_id_priority`,
//! `is_partition_entry`) serve both discover (scanning attached braid-labeled
//! disks) and recover (resolving stable identifiers for live pool devices). The
//! `ByIdResolver` trait (enumeration + canonicalization) serves recover, which
//! needs a mockable seam; discover reads and canonicalizes its injectable by-id
//! directory directly via `std::fs` and does not use the trait.

/// Resolve `/dev/disk/by-id/` symlinks for recover.
///
/// Discover does not use this trait -- it reads and canonicalizes its injectable
/// by-id directory directly via `std::fs`, driving real udev-style symlinks in a
/// tempdir under test. Recover is the trait's only consumer and substitutes a
/// mock at this boundary. Kept separate from `probe::Filesystem` so tests can
/// substitute this narrow boundary without widening a shared trait that already
/// has many mock impls. `RealByIdResolver` is the production implementation.
pub trait ByIdResolver {
    /// List filenames under `/dev/disk/by-id/`. Returns an empty vec if the
    /// directory does not exist (mirrors `Filesystem::list_dir` semantics).
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error>;

    /// Canonicalize `path` (resolve all symlinks to an absolute path).
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error>;
}

/// Production by-id resolver that reads udev symlinks from the real
/// filesystem; tests substitute their own resolver implementation.
pub struct RealByIdResolver;

impl ByIdResolver for RealByIdResolver {
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error> {
        match std::fs::read_dir("/dev/disk/by-id") {
            Ok(entries) => entries
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())
    }
}

/// Priority for /dev/disk/by-id/ symlink prefixes. Lower = more preferred.
///
/// | Prefix  | Source                                               | Stable?                           |
/// |---------|------------------------------------------------------|-----------------------------------|
/// | wwn-    | World Wide Name from firmware, fully persistent      | Yes                               |
/// | nvme-   | NVMe controller serial + namespace                   | Yes                               |
/// | scsi-   | SCSI Inquiry VPD page (hardware serial/EUI-64)       | Yes                               |
/// | ata-    | Model + serial via kernel ATA driver                 | Yes (format can vary by kernel)   |
/// | usb-    | USB device serial number                             | Yes (absent on cheap drives)      |
/// | other   | Everything else (dm-uuid, etc.)                      | Varies                            |
pub(crate) fn by_id_priority(filename: &str) -> u8 {
    if filename.starts_with("wwn-") {
        return 0;
    }
    if filename.starts_with("nvme-") {
        return 1;
    }
    if filename.starts_with("scsi-") {
        return 2;
    }
    if filename.starts_with("ata-") {
        return 3;
    }
    if filename.starts_with("usb-") {
        return 4;
    }
    5
}

/// Filter udev `-partN` by-id aliases so callers operate on whole-disk
/// identifiers rather than partition symlinks.
pub(crate) fn is_partition_entry(name: &str) -> bool {
    // Match -part1, -part2, etc. at end of name.
    if let Some(idx) = name.rfind("-part") {
        let rest = &name[idx + 5..];
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::ByIdResolver;
    use std::collections::BTreeMap;

    /// Test resolver for `ByIdResolver`. `entries` is what
    /// `list_by_id_entries` returns; `canonicalize_results` is the symlink to
    /// canonical-path map used by `canonicalize`. Unmocked paths return
    /// `NotFound`.
    #[derive(Default)]
    pub(crate) struct MockByIdResolver {
        entries: Vec<String>,
        canonicalize_results: BTreeMap<String, String>,
    }

    impl MockByIdResolver {
        pub(crate) fn with_entries<I, S>(mut self, entries: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            self.entries = entries.into_iter().map(Into::into).collect();
            self
        }

        pub(crate) fn with_canonical(mut self, path: &str, target: &str) -> Self {
            self.canonicalize_results
                .insert(path.to_string(), target.to_string());
            self
        }
    }

    impl ByIdResolver for MockByIdResolver {
        fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error> {
            Ok(self.entries.clone())
        }

        fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
            self.canonicalize_results.get(path).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, format!("mock: {path}"))
            })
        }
    }

    /// Build a `MockByIdResolver` from `(underlying, by_id_filename)` pairs.
    /// For each pair, the by-id entry is registered and both the entry and the
    /// underlying canonicalize to the underlying path. Use this for success-path
    /// tests where the resolver should find a matching entry per pool device.
    pub(crate) fn resolver_for(mappings: &[(&str, &str)]) -> MockByIdResolver {
        let mut resolver = MockByIdResolver::default();
        for (underlying, filename) in mappings {
            resolver.entries.push((*filename).to_string());
            resolver.canonicalize_results.insert(
                format!("/dev/disk/by-id/{filename}"),
                (*underlying).to_string(),
            );
            resolver
                .canonicalize_results
                .insert((*underlying).to_string(), (*underlying).to_string());
        }
        resolver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_detection() {
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part1"));
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part12"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-part"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-partial"));
    }

    #[test]
    fn by_id_priority_ordering() {
        /*
         * Intent: verify the relative priority of all known by-id prefix classes.
         * Why it exists: if the ordering constants are wrong (e.g. ata and scsi swapped),
         *   discover would silently prefer the less stable symlink.
         * Scenario: developer adds a new prefix tier and accidentally misorders the values.
         */
        assert!(by_id_priority("wwn-0x123") < by_id_priority("nvme-SAMSUNG"));
        assert!(by_id_priority("nvme-SAMSUNG") < by_id_priority("scsi-360014"));
        assert!(by_id_priority("scsi-360014") < by_id_priority("ata-SEAGATE"));
        assert!(by_id_priority("ata-WD") < by_id_priority("usb-Kingston"));
        assert!(by_id_priority("usb-Kingston") < by_id_priority("dm-uuid-123"));
    }
}
