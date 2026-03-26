use crate::types::{MapperName, MountPoint};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigBuildError {
    #[error("mount_point must not be empty")]
    EmptyMountPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    mount_point: MountPoint,
}

impl Config {
    pub fn new(mount_point: MountPoint) -> Result<Self, ConfigBuildError> {
        if mount_point.0.is_empty() {
            return Err(ConfigBuildError::EmptyMountPoint);
        }
        Ok(Config { mount_point })
    }

    pub fn mount_point(&self) -> &MountPoint {
        &self.mount_point
    }
}

/// Returns the mapper name for a disk name: braid-<name>
pub fn mapper_name(name: &str) -> MapperName {
    MapperName(format!("braid-{name}"))
}

#[derive(Deserialize)]
struct RawConfig {
    mount_point: MountPoint,
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigBuildError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        Config::new(raw.mount_point)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let raw = r#"{"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.mount_point().as_str(), "/mnt/storage");
    }

    #[test]
    fn rejects_empty_mount_point() {
        let raw = r#"{"mount_point":""}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("empty mount should fail");
        assert!(err.to_string().contains("mount_point must not be empty"));
    }

    #[test]
    fn mapper_name_for_disk() {
        assert_eq!(mapper_name("toshiba"), MapperName("braid-toshiba".into()));
        assert_eq!(mapper_name("ironwolf"), MapperName("braid-ironwolf".into()));
    }
}
