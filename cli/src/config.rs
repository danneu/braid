use crate::types::{DiskName, LuksLabel, MapperName, MountPoint};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use thiserror::Error;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/braid/config.json";

#[derive(Debug, Error)]
pub enum ConfigBuildError {
    #[error("mount_point must not be empty")]
    EmptyMountPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pwm {
    pub platform_device: String,
    pub number: u8,
    pub min_start: u8,
    pub max_stop: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FanControl {
    pub pwm: Pwm,
    pub min_temp: u8,
    pub max_temp: u8,
    pub min_fan_speed_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ups {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    mount_point: MountPoint,
    storage_group: Option<String>,
    fan_control: Option<FanControl>,
    ups: Option<Ups>,
}

impl Config {
    pub fn new(mount_point: MountPoint) -> Result<Self, ConfigBuildError> {
        if mount_point.0.is_empty() {
            return Err(ConfigBuildError::EmptyMountPoint);
        }
        Ok(Config {
            mount_point,
            storage_group: None,
            fan_control: None,
            ups: None,
        })
    }

    pub fn mount_point(&self) -> &MountPoint {
        &self.mount_point
    }

    /// Optional Unix group that receives write access on the mounted pool root.
    pub fn storage_group(&self) -> Option<&str> {
        self.storage_group.as_deref()
    }

    pub fn fan_control(&self) -> Option<&FanControl> {
        self.fan_control.as_ref()
    }

    pub fn ups(&self) -> Option<&Ups> {
        self.ups.as_ref()
    }
}

/// Returns the mapper name for a disk: braid-<name>. Validated-type
/// signature so callers cannot synthesize a `MapperName` from unchecked
/// text at this boundary.
pub fn mapper_name(name: &DiskName) -> MapperName {
    MapperName(format!("braid-{}", name.as_str()))
}

/// Thin public entry point for the canonical LUKS label constructor.
/// Kept beside `mapper_name` so call sites use symmetric helpers for
/// both braid-owned names.
pub fn luks_label_for(name: &DiskName) -> LuksLabel {
    LuksLabel::for_disk(name)
}

/// Display-only mapper parser for diagnostics and explicit carve-outs.
/// Identity decisions must use LUKS UUID/devid membership instead.
pub fn name_from_mapper(mapper: &str) -> Option<&str> {
    mapper.strip_prefix("braid-")
}

#[derive(Deserialize)]
struct RawConfig {
    mount_point: MountPoint,
    #[serde(default)]
    storage_group: Option<String>,
    #[serde(default)]
    fan_control: Option<FanControl>,
    #[serde(default)]
    ups: Option<Ups>,
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigBuildError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let mut cfg = Config::new(raw.mount_point)?;
        cfg.storage_group = raw.storage_group;
        cfg.fan_control = raw.fan_control;
        cfg.ups = raw.ups;
        Ok(cfg)
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
        let raw = r#"{"mount_point":"/mnt/storage","storage_group":"storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.mount_point().as_str(), "/mnt/storage");
        assert_eq!(cfg.storage_group(), Some("storage"));
    }

    #[test]
    fn rejects_empty_mount_point() {
        let raw = r#"{"mount_point":""}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("empty mount should fail");
        assert!(err.to_string().contains("mount_point must not be empty"));
    }

    // Intent: `mapper_name(&DiskName)` returns the canonical
    // `braid-<name>` mapper name for representative disk names.
    // Why it exists: pins the mapper-prefix convention at the helper
    // boundary so argv builders and path constructors do not drift.
    // Scenario: planner code holds a validated `DiskName` and asks for
    // the mapper basename used in `/dev/mapper/<X>` paths.
    #[test]
    fn mapper_name_for_disk() {
        let toshiba = DiskName::parse("toshiba").unwrap();
        let ironwolf = DiskName::parse("ironwolf").unwrap();
        assert_eq!(mapper_name(&toshiba), MapperName("braid-toshiba".into()));
        assert_eq!(mapper_name(&ironwolf), MapperName("braid-ironwolf".into()));
    }

    // Intent: `luks_label_for(&DiskName)` returns the canonical
    // `braid-<name>` LUKS label for representative disk names.
    // Why it exists: pins the label-prefix convention at the helper
    // boundary instead of at every downstream cryptsetup argv site.
    // Scenario: planner, executor, and recovery code hold a validated
    // `DiskName` and need the matching LUKS2 header label.
    #[test]
    fn luks_label_for_disk() {
        let toshiba = DiskName::parse("toshiba").unwrap();
        let ironwolf = DiskName::parse("ironwolf").unwrap();
        assert_eq!(luks_label_for(&toshiba).as_str(), "braid-toshiba");
        assert_eq!(luks_label_for(&ironwolf).as_str(), "braid-ironwolf");
    }

    #[test]
    fn name_from_mapper_strips_prefix() {
        assert_eq!(name_from_mapper("braid-toshiba"), Some("toshiba"));
        assert_eq!(name_from_mapper("braid-ironwolf"), Some("ironwolf"));
    }

    #[test]
    fn name_from_mapper_returns_none_for_non_braid() {
        assert_eq!(name_from_mapper("luks-something"), None);
        assert_eq!(name_from_mapper(""), None);
    }

    // Intent: Config deserializes when fan_control is absent from JSON.
    // Why: fan_control is opt-in on the Nix side; absent JSON key means
    // braid.fanControl.enable = false. Config::fan_control must return None.
    // Scenario: Config.json written by a NixOS generation without the
    // fanControl module enabled.
    #[test]
    fn parses_config_without_fan_control() {
        let raw = r#"{"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.mount_point().as_str(), "/mnt/storage");
        assert!(cfg.fan_control().is_none());
    }

    // Intent: Config deserializes the full fan_control shape from JSON.
    // Why: modules/braid/cli.nix emits the exact key names (snake_case) below;
    // a mismatch silently leaves Config::fan_control as None and the TUI
    // degrades to "disabled" without a visible error.
    // Scenario: NixOS generated /etc/braid/config.json with fanControl enabled
    // and calibration values from hddfancontrol pwm-test.
    #[test]
    fn parses_config_with_fan_control() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "fan_control": {
                "pwm": {
                    "platform_device": "f71882fg.656",
                    "number": 2,
                    "min_start": 70,
                    "max_stop": 60
                },
                "min_temp": 30,
                "max_temp": 40,
                "min_fan_speed_percent": 20
            }
        }"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        let fc = cfg.fan_control().expect("fan_control should be Some");
        assert_eq!(fc.pwm.platform_device, "f71882fg.656");
        assert_eq!(fc.pwm.number, 2);
        assert_eq!(fc.pwm.min_start, 70);
        assert_eq!(fc.pwm.max_stop, 60);
        assert_eq!(fc.min_temp, 30);
        assert_eq!(fc.max_temp, 40);
        assert_eq!(fc.min_fan_speed_percent, 20);
    }

    // Intent: Config deserializes the ups block emitted by modules/braid/cli.nix.
    // Why: cli.nix writes `ups = { name }` when braid.ups.enable = true;
    // a schema mismatch would stop UPS-aware commands from seeing the NUT
    // daemon name they need to query.
    // Scenario: NixOS generation with braid.ups.enable = true and name = "ups".
    #[test]
    fn parses_config_with_ups() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "ups": { "name": "ups" }
        }"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        let u = cfg.ups().expect("ups should be Some");
        assert_eq!(u.name, "ups");
    }

    // Intent: Config rejects the legacy UPS shape with an `enable` field.
    // Why: accepting stale JSON would turn a hand-edited
    // `{"enable":false}` block into a live UPS config.
    // Scenario: operator keeps an old config.json after the JSON schema
    // switches to presence-only UPS enablement.
    #[test]
    fn rejects_config_with_legacy_ups_enable_field() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "ups": { "enable": true, "name": "ups" }
        }"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("legacy ups shape must fail");
        let message = err.to_string();
        assert!(
            message.contains("enable") || message.contains("unknown field"),
            "expected legacy-field error, got: {message}"
        );
    }

    // Intent: Config parses when ups key is absent (braid.ups.enable = false).
    // Why: absent JSON key means no UPS configured; preflight must treat that
    // as "no UPS check needed" rather than erroring at config-read time.
    #[test]
    fn parses_config_without_ups() {
        let raw = r#"{"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert!(cfg.ups().is_none());
    }

    // Intent: malformed pwm (missing required fields) fails to deserialize.
    // Why: catching a missing pwm field at parse time surfaces the config bug
    // at CLI startup rather than as a mysterious None later in the TUI.
    // Scenario: hand-edited config.json or a future cli.nix refactor drops
    // a required pwm key.
    #[test]
    fn rejects_malformed_pwm() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "fan_control": {
                "pwm": {"platform_device": "f71882fg.656"},
                "min_temp": 30,
                "max_temp": 40,
                "min_fan_speed_percent": 20
            }
        }"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("malformed pwm must fail");
        assert!(
            err.to_string().contains("number")
                || err.to_string().contains("min_start")
                || err.to_string().contains("max_stop"),
            "expected missing-field error, got: {err}"
        );
    }
}
