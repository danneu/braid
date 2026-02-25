use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const DISK_MAP_FILE: &str = "/var/lib/braid/disk-map.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskMap {
    pub schema_version: u32,
    pub disks: BTreeMap<String, DiskMapEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskMapEntry {
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: u64,
    pub added_at: String,
}

impl DiskMap {
    pub fn new() -> Self {
        DiskMap {
            schema_version: 1,
            disks: BTreeMap::new(),
        }
    }
}

/// Load disk map from the production path.
pub fn load_disk_map() -> DiskMap {
    load_disk_map_at(Path::new(DISK_MAP_FILE))
}

/// Load disk map from an arbitrary path (for testing).
/// Returns an empty map if the file is missing or unparseable.
pub fn load_disk_map_at(path: &Path) -> DiskMap {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return DiskMap::new(),
    };
    serde_json::from_str(&contents).unwrap_or_else(|_| DiskMap::new())
}

/// Save disk map to the production path (atomic: tmp + rename).
pub fn save_disk_map(map: &DiskMap) -> Result<(), std::io::Error> {
    save_disk_map_at(Path::new(DISK_MAP_FILE), map)
}

/// Save disk map to an arbitrary path (for testing). Atomic: tmp + rename in same dir.
pub fn save_disk_map_at(path: &Path, map: &DiskMap) -> Result<(), std::io::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let json =
        serde_json::to_string_pretty(map).map_err(std::io::Error::other)?;

    let tmp = format!("{}.tmp", path.display());
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Upsert a disk entry in the map.
pub fn record_disk(
    map: &mut DiskMap,
    name: &str,
    by_id: &str,
    luks_uuid: &str,
    devid: u64,
) {
    map.disks.insert(
        name.to_owned(),
        DiskMapEntry {
            by_id: by_id.to_owned(),
            luks_uuid: luks_uuid.to_owned(),
            devid,
            added_at: now_iso(),
        },
    );
}

/// Remove a disk entry by name.
pub fn remove_disk(map: &mut DiskMap, name: &str) {
    map.disks.remove(name);
}

/// Remove all entries whose devid is in the given set.
/// Used by remove-missing --missing-id flows.
pub fn remove_disks_by_devids(map: &mut DiskMap, devids: &[u64]) {
    map.disks.retain(|_, entry| !devids.contains(&entry.devid));
}

/// Remove entries whose devid is NOT in the given set of live devids.
/// Used by remove-missing (prune stale entries after eviction).
pub fn prune_absent_devids(map: &mut DiskMap, live_devids: &[u64]) {
    map.disks
        .retain(|_, entry| live_devids.contains(&entry.devid));
}

fn now_iso() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".into())
}

/// Best-effort disk map update. Logs warning on failure, never fails the caller.
pub fn update_disk_map_best_effort(f: impl FnOnce(&mut DiskMap)) {
    let mut map = load_disk_map();
    f(&mut map);
    if let Err(e) = save_disk_map(&map) {
        eprintln!("Warning: failed to update disk map: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn map_path(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("disk-map.json")
    }

    #[test]
    fn load_missing_file_returns_empty_map() {
        let dir = TempDir::new().unwrap();
        let path = map_path(&dir);
        let map = load_disk_map_at(&path);
        assert_eq!(map.schema_version, 1);
        assert!(map.disks.is_empty());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = map_path(&dir);

        let mut map = DiskMap::new();
        record_disk(
            &mut map,
            "toshiba",
            "/dev/disk/by-id/ata-Toshiba_1",
            "aaaa-bbbb",
            1,
        );

        save_disk_map_at(&path, &map).unwrap();
        let reloaded = load_disk_map_at(&path);

        assert_eq!(reloaded.schema_version, 1);
        assert_eq!(reloaded.disks.len(), 1);
        let entry = &reloaded.disks["toshiba"];
        assert_eq!(entry.by_id, "/dev/disk/by-id/ata-Toshiba_1");
        assert_eq!(entry.luks_uuid, "aaaa-bbbb");
        assert_eq!(entry.devid, 1);
        assert!(!entry.added_at.is_empty());
    }

    #[test]
    fn record_disk_upsert_overwrites() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/old", "uuid-old", 1);
        record_disk(&mut map, "d1", "/by-id/new", "uuid-new", 5);

        assert_eq!(map.disks.len(), 1);
        assert_eq!(map.disks["d1"].by_id, "/by-id/new");
        assert_eq!(map.disks["d1"].luks_uuid, "uuid-new");
        assert_eq!(map.disks["d1"].devid, 5);
    }

    #[test]
    fn remove_disk_removes_entry() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/1", "u1", 1);
        record_disk(&mut map, "d2", "/by-id/2", "u2", 2);

        remove_disk(&mut map, "d1");

        assert_eq!(map.disks.len(), 1);
        assert!(!map.disks.contains_key("d1"));
        assert!(map.disks.contains_key("d2"));
    }

    #[test]
    fn remove_disk_noop_for_missing_name() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/1", "u1", 1);

        remove_disk(&mut map, "nonexistent");

        assert_eq!(map.disks.len(), 1);
    }

    #[test]
    fn remove_disks_by_devids_removes_matching() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/1", "u1", 1);
        record_disk(&mut map, "d2", "/by-id/2", "u2", 2);
        record_disk(&mut map, "d3", "/by-id/3", "u3", 3);

        remove_disks_by_devids(&mut map, &[2, 3]);

        assert_eq!(map.disks.len(), 1);
        assert!(map.disks.contains_key("d1"));
    }

    #[test]
    fn remove_disks_by_devids_noop_for_no_match() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/1", "u1", 1);

        remove_disks_by_devids(&mut map, &[99]);

        assert_eq!(map.disks.len(), 1);
    }

    #[test]
    fn prune_absent_devids_keeps_only_live() {
        let mut map = DiskMap::new();
        record_disk(&mut map, "d1", "/by-id/1", "u1", 1);
        record_disk(&mut map, "d2", "/by-id/2", "u2", 2);
        record_disk(&mut map, "d3", "/by-id/3", "u3", 3);

        prune_absent_devids(&mut map, &[1, 3]);

        assert_eq!(map.disks.len(), 2);
        assert!(map.disks.contains_key("d1"));
        assert!(map.disks.contains_key("d3"));
        assert!(!map.disks.contains_key("d2"));
    }

    #[test]
    fn load_corrupted_file_returns_empty_map() {
        let dir = TempDir::new().unwrap();
        let path = map_path(&dir);
        std::fs::write(&path, "not json at all").unwrap();

        let map = load_disk_map_at(&path);
        assert_eq!(map.schema_version, 1);
        assert!(map.disks.is_empty());
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deep").join("disk-map.json");

        let map = DiskMap::new();
        save_disk_map_at(&path, &map).unwrap();

        let reloaded = load_disk_map_at(&path);
        assert_eq!(reloaded.schema_version, 1);
    }
}
