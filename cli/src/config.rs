use crate::types::{ByIdPath, MapperName};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigBuildError {
    #[error("disks must not be empty")]
    EmptyDisks,
    #[error("duplicate by_id value in config: {0}")]
    DuplicateByIdValue(String),
    #[error("mount_point must not be empty")]
    EmptyMountPoint,
    #[error("invalid disk key '{0}': must start with a letter and contain only letters, digits, hyphens, or underscores")]
    InvalidDiskKey(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiskConfig {
    pub by_id: ByIdPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    disks: BTreeMap<String, DiskConfig>,
    mount_point: String,
}

impl Config {
    pub fn new(
        disks: BTreeMap<String, DiskConfig>,
        mount_point: String,
    ) -> Result<Self, ConfigBuildError> {
        if disks.is_empty() {
            return Err(ConfigBuildError::EmptyDisks);
        }
        for name in disks.keys() {
            if !is_valid_disk_key(name) {
                return Err(ConfigBuildError::InvalidDiskKey(name.clone()));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (_, disk) in &disks {
            if !seen.insert(&disk.by_id) {
                return Err(ConfigBuildError::DuplicateByIdValue(
                    disk.by_id.to_string(),
                ));
            }
        }
        if mount_point.is_empty() {
            return Err(ConfigBuildError::EmptyMountPoint);
        }
        Ok(Config { disks, mount_point })
    }

    pub fn disks(&self) -> &BTreeMap<String, DiskConfig> {
        &self.disks
    }

    pub fn disk_by_name(&self, name: &str) -> Option<&DiskConfig> {
        self.disks.get(name)
    }

    pub fn names(&self) -> Vec<&String> {
        self.disks.keys().collect()
    }

    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }
}

/// Returns the mapper name for a named disk: braid-<name>
pub fn mapper_name(name: &str) -> MapperName {
    MapperName(format!("braid-{name}"))
}

fn is_valid_disk_key(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Deserialize)]
struct RawConfig {
    disks: BTreeMap<String, DiskConfig>,
    mount_point: String,
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigBuildError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        Config::new(raw.disks, raw.mount_point)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
}

pub fn config_read(path: &Path) -> Result<Config, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;

    let cfg: Config = serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;

    Ok(cfg)
}

pub fn config_read_raw(path: &Path) -> Result<(Config, String), ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;

    let cfg: Config = serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;

    Ok((cfg, raw))
}

pub fn config_hash(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(raw.as_bytes());
    format!("sha256:{:x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let raw = r#"{"disks":{"toshiba":{"by_id":"/dev/disk/by-id/a"}},"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.disks().len(), 1);
        assert!(cfg.disk_by_name("toshiba").is_some());
        assert_eq!(cfg.mount_point(), "/mnt/storage");
    }

    #[test]
    fn config_hash_uses_sha256_prefix() {
        let h = config_hash("anything");
        assert!(h.starts_with("sha256:"), "expected sha256: prefix, got: {h}");
        let hex = &h["sha256:".len()..];
        assert_eq!(hex.len(), 64, "expected 64 hex chars, got {}", hex.len());
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn rejects_duplicate_by_id_values() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "a".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/same".to_owned()),
            },
        );
        disks.insert(
            "b".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/same".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("duplicate by_id values should fail");
        assert!(matches!(err, ConfigBuildError::DuplicateByIdValue(_)));
    }

    #[test]
    fn rejects_empty_disks() {
        let err = Config::new(BTreeMap::new(), "/mnt/storage".to_owned())
            .expect_err("empty disks should fail");
        assert!(matches!(err, ConfigBuildError::EmptyDisks));
    }

    #[test]
    fn rejects_empty_disks_json() {
        let raw = r#"{"disks":{},"mount_point":"/mnt/storage"}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("empty disks JSON should fail");
        assert!(
            err.to_string().contains("disks must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mapper_name_for_disk() {
        assert_eq!(mapper_name("toshiba"), MapperName("braid-toshiba".into()));
        assert_eq!(mapper_name("ironwolf"), MapperName("braid-ironwolf".into()));
    }

    #[test]
    fn names_returns_sorted_keys() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "zebra".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/z".to_owned()),
            },
        );
        disks.insert(
            "alpha".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let cfg = Config::new(disks, "/mnt/storage".to_owned()).unwrap();
        let names: Vec<&str> = cfg.names().into_iter().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn rejects_disk_key_starting_with_digit() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "1bad".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("digit-starting name should fail");
        assert!(matches!(err, ConfigBuildError::InvalidDiskKey(_)));
    }

    #[test]
    fn rejects_disk_key_starting_with_hyphen() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "-bad".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("hyphen-starting name should fail");
        assert!(matches!(err, ConfigBuildError::InvalidDiskKey(_)));
    }

    #[test]
    fn rejects_disk_key_starting_with_underscore() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "_bad".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("underscore-starting name should fail");
        assert!(matches!(err, ConfigBuildError::InvalidDiskKey(_)));
    }

    #[test]
    fn rejects_disk_key_with_space() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "my disk".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("space in name should fail");
        assert!(matches!(err, ConfigBuildError::InvalidDiskKey(_)));
    }

    #[test]
    fn rejects_empty_disk_key() {
        let mut disks = BTreeMap::new();
        disks.insert(
            "".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        let err = Config::new(disks, "/mnt/storage".to_owned())
            .expect_err("empty name should fail");
        assert!(matches!(err, ConfigBuildError::InvalidDiskKey(_)));
    }

    #[test]
    fn accepts_valid_disk_keys() {
        for name in ["toshiba", "disk1", "my-disk", "my_disk", "A", "Z1-b2-c3"] {
            let mut disks = BTreeMap::new();
            disks.insert(
                name.to_owned(),
                DiskConfig {
                    by_id: ByIdPath(format!("/dev/disk/by-id/{name}")),
                },
            );
            Config::new(disks, "/mnt/storage".to_owned())
                .unwrap_or_else(|e| panic!("name '{name}' should be valid, got: {e}"));
        }
    }
}
