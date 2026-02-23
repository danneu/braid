use crate::types::ByIdPath;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub disks: Vec<ByIdPath>,
    #[serde(rename = "mountPoint")]
    pub mount_point: String,
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
    #[error("config validation failed: {0}")]
    Validation(String),
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

    validate(&cfg)?;
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

    validate(&cfg)?;
    Ok((cfg, raw))
}

pub fn config_hash(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(raw.as_bytes());
    format!("sha256:{:x}", hash)
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    if cfg.disks.is_empty() {
        return Err(ConfigError::Validation("disks must not be empty".to_owned()));
    }
    if cfg.mount_point.is_empty() {
        return Err(ConfigError::Validation(
            "mountPoint must not be empty".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let raw = r#"{"disks":["/dev/disk/by-id/a"],"mountPoint":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.disks.len(), 1);
        assert_eq!(cfg.mount_point, "/mnt/storage");
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
    fn rejects_empty_disks() {
        let cfg = Config {
            disks: vec![],
            mount_point: "/mnt/storage".to_owned(),
        };
        let err = validate(&cfg).expect_err("empty disks should fail");
        assert!(matches!(err, ConfigError::Validation(_)));
    }
}
