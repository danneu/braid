use crate::state_io;
use crate::types::ByIdPath;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

const POOL_PATH: &str = "/var/lib/braid/pool.json";

#[derive(Debug, Error)]
pub enum MembershipError {
    #[error("pool membership not found at {0} — run 'braid add' to create a pool or 'braid discover --write' to rebuild from existing disks")]
    NotFound(String),
    #[error("pool membership file corrupt at {0}: {1}")]
    Corrupt(String, String),
    #[error("failed to write pool membership: {0}")]
    Write(#[source] std::io::Error),
    #[error("membership conflict: {0}")]
    Conflict(String),
    #[error("invalid disk name '{0}': must start with a letter, contain only letters, digits, hyphens, or underscores, and be at most 32 characters")]
    InvalidDiskName(String),
    #[error("invalid by_id path '{0}': must start with /dev/disk/by-id/")]
    InvalidByIdPath(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMembership {
    pub disks: BTreeMap<String, ByIdPath>,
}

impl PoolMembership {
    pub fn empty() -> Self {
        PoolMembership {
            disks: BTreeMap::new(),
        }
    }
}

/// Load authoritative pool membership from disk.
/// Returns NotFound if the file doesn't exist, Corrupt if it exists but can't be parsed.
pub fn load_membership() -> Result<PoolMembership, MembershipError> {
    load_membership_from(Path::new(POOL_PATH))
}

pub fn load_membership_from(path: &Path) -> Result<PoolMembership, MembershipError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MembershipError::NotFound(path.display().to_string()));
        }
        Err(e) => {
            return Err(MembershipError::Corrupt(
                path.display().to_string(),
                e.to_string(),
            ));
        }
    };

    serde_json::from_str::<PoolMembership>(&raw)
        .map_err(|e| MembershipError::Corrupt(path.display().to_string(), e.to_string()))
}

/// Durably persist pool membership. Fails hard on any I/O error.
pub fn save_membership(m: &PoolMembership) -> Result<(), MembershipError> {
    save_membership_to(m, Path::new(POOL_PATH))
}

pub fn save_membership_to(m: &PoolMembership, path: &Path) -> Result<(), MembershipError> {
    let json = serde_json::to_string_pretty(m).expect("PoolMembership serialization cannot fail");
    state_io::atomic_write(path, json.as_bytes()).map_err(MembershipError::Write)
}

/// Validate that adding (name, by_id) doesn't conflict with existing membership.
/// Rejects: same name with different by_id, same by_id under different name.
pub fn validate_no_conflicts(
    existing: &PoolMembership,
    name: &str,
    by_id: &str,
) -> Result<(), MembershipError> {
    // Check name reassignment: name exists with different by_id
    if let Some(current_by_id) = existing.disks.get(name) {
        if current_by_id.0 != by_id {
            return Err(MembershipError::Conflict(format!(
                "disk '{}' already exists with by_id '{}', cannot reassign to '{}'",
                name, current_by_id, by_id
            )));
        }
    }

    // Check by_id rename: by_id exists under different name
    for (existing_name, existing_by_id) in &existing.disks {
        if existing_by_id.0 == by_id && existing_name != name {
            return Err(MembershipError::Conflict(format!(
                "by_id '{}' already registered under name '{}', cannot register as '{}'",
                by_id, existing_name, name
            )));
        }
    }

    Ok(())
}

pub fn is_valid_disk_name(name: &str) -> bool {
    if name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn validate_disk_name(name: &str) -> Result<(), MembershipError> {
    if !is_valid_disk_name(name) {
        return Err(MembershipError::InvalidDiskName(name.to_owned()));
    }
    Ok(())
}

pub fn validate_by_id(by_id: &str) -> Result<(), MembershipError> {
    if !by_id.starts_with("/dev/disk/by-id/") {
        return Err(MembershipError::InvalidByIdPath(by_id.to_owned()));
    }
    Ok(())
}

/// Parse a "name=by_id" disk spec from CLI arguments.
pub fn parse_disk_spec(spec: &str) -> Result<(String, ByIdPath), MembershipError> {
    let (name, by_id) = spec.split_once('=').ok_or_else(|| {
        MembershipError::InvalidDiskName(format!(
            "expected NAME=/dev/disk/by-id/..., got '{}'",
            spec
        ))
    })?;
    validate_disk_name(name)?;
    validate_by_id(by_id)?;
    Ok((name.to_owned(), ByIdPath(by_id.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");

        let mut m = PoolMembership::empty();
        m.disks.insert(
            "toshiba".into(),
            ByIdPath("/dev/disk/by-id/ata-TOSHIBA".into()),
        );
        m.disks
            .insert("wd".into(), ByIdPath("/dev/disk/by-id/ata-WDC".into()));

        save_membership_to(&m, &path).unwrap();
        let loaded = load_membership_from(&path).unwrap();
        assert_eq!(m, loaded);
    }

    #[test]
    fn not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let err = load_membership_from(&path).unwrap_err();
        assert!(matches!(err, MembershipError::NotFound(_)));
    }

    #[test]
    fn corrupt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("pool.json");
        std::fs::write(&path, "not json").unwrap();
        let err = load_membership_from(&path).unwrap_err();
        assert!(matches!(err, MembershipError::Corrupt(_, _)));
    }

    #[test]
    fn conflict_name_reassignment() {
        let mut m = PoolMembership::empty();
        m.disks
            .insert("toshiba".into(), ByIdPath("/dev/disk/by-id/ata-OLD".into()));

        let err = validate_no_conflicts(&m, "toshiba", "/dev/disk/by-id/ata-NEW").unwrap_err();
        assert!(matches!(err, MembershipError::Conflict(_)));
        assert!(err.to_string().contains("cannot reassign"));
    }

    #[test]
    fn conflict_by_id_rename() {
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "toshiba".into(),
            ByIdPath("/dev/disk/by-id/ata-SAME".into()),
        );

        let err = validate_no_conflicts(&m, "newname", "/dev/disk/by-id/ata-SAME").unwrap_err();
        assert!(matches!(err, MembershipError::Conflict(_)));
        assert!(err.to_string().contains("cannot register"));
    }

    #[test]
    fn no_conflict_same_entry() {
        let mut m = PoolMembership::empty();
        m.disks
            .insert("toshiba".into(), ByIdPath("/dev/disk/by-id/ata-X".into()));

        // Re-adding same name + same by_id is fine (idempotent)
        validate_no_conflicts(&m, "toshiba", "/dev/disk/by-id/ata-X").unwrap();
    }

    #[test]
    fn no_conflict_new_entry() {
        let mut m = PoolMembership::empty();
        m.disks
            .insert("toshiba".into(), ByIdPath("/dev/disk/by-id/ata-X".into()));

        validate_no_conflicts(&m, "wd", "/dev/disk/by-id/ata-Y").unwrap();
    }

    #[test]
    fn valid_disk_names() {
        for name in ["toshiba", "disk1", "my-disk", "my_disk", "A", "Z1-b2-c3"] {
            assert!(is_valid_disk_name(name), "'{name}' should be valid");
        }
    }

    #[test]
    fn invalid_disk_names() {
        for name in ["1bad", "-bad", "_bad", "my disk", "", &"a".repeat(33)] {
            assert!(!is_valid_disk_name(name), "'{name}' should be invalid");
        }
    }

    #[test]
    fn parse_disk_spec_valid() {
        let (name, by_id) = parse_disk_spec("toshiba=/dev/disk/by-id/ata-TOSHIBA").unwrap();
        assert_eq!(name, "toshiba");
        assert_eq!(by_id.0, "/dev/disk/by-id/ata-TOSHIBA");
    }

    #[test]
    fn parse_disk_spec_no_equals() {
        let err = parse_disk_spec("toshiba").unwrap_err();
        assert!(err.to_string().contains("expected NAME="));
    }

    #[test]
    fn parse_disk_spec_bad_by_id() {
        let err = parse_disk_spec("toshiba=/dev/sda").unwrap_err();
        assert!(matches!(err, MembershipError::InvalidByIdPath(_)));
    }

    #[test]
    fn parse_disk_spec_bad_name() {
        let err = parse_disk_spec("1bad=/dev/disk/by-id/ata-X").unwrap_err();
        assert!(matches!(err, MembershipError::InvalidDiskName(_)));
    }
}
