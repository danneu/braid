use crate::types::ByIdPath;
use serde::Deserialize;
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigBuildError {
    #[error("disks must not be empty")]
    EmptyDisks,
    #[error("duplicate disk in config: {0}")]
    DuplicateDisk(String),
    #[error("mountPoint must not be empty")]
    EmptyMountPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    disks: Vec<ByIdPath>,
    mount_point: String,
}

impl Config {
    pub fn new(disks: Vec<ByIdPath>, mount_point: String) -> Result<Self, ConfigBuildError> {
        if disks.is_empty() {
            return Err(ConfigBuildError::EmptyDisks);
        }
        let mut seen = std::collections::HashSet::new();
        for disk in &disks {
            if !seen.insert(disk) {
                return Err(ConfigBuildError::DuplicateDisk(disk.to_string()));
            }
        }
        if mount_point.is_empty() {
            return Err(ConfigBuildError::EmptyMountPoint);
        }
        Ok(Config { disks, mount_point })
    }

    pub fn disks(&self) -> &[ByIdPath] {
        &self.disks
    }

    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }
}

#[derive(Deserialize)]
struct RawConfig {
    disks: Vec<ByIdPath>,
    #[serde(rename = "mountPoint")]
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
        let raw = r#"{"disks":["/dev/disk/by-id/a"],"mountPoint":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.disks().len(), 1);
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
    fn rejects_duplicate_disks() {
        let err = Config::new(
            vec![
                ByIdPath("/dev/disk/by-id/a".to_owned()),
                ByIdPath("/dev/disk/by-id/a".to_owned()),
            ],
            "/mnt/storage".to_owned(),
        )
        .expect_err("duplicate disks should fail");
        assert!(matches!(err, ConfigBuildError::DuplicateDisk(_)));
    }

    #[test]
    fn rejects_empty_disks() {
        let err = Config::new(vec![], "/mnt/storage".to_owned())
            .expect_err("empty disks should fail");
        assert!(matches!(err, ConfigBuildError::EmptyDisks));
    }

    #[test]
    fn rejects_empty_disks_json() {
        let raw = r#"{"disks":[],"mountPoint":"/mnt/storage"}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("empty disks JSON should fail");
        assert!(
            err.to_string().contains("disks must not be empty"),
            "unexpected error: {err}"
        );
    }
}
