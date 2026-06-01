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

/// braid-owned config schema written by `modules/braid/cli.nix`; nested under
/// `RawConfig` whose `deny_unknown_fields` does not propagate.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pwm {
    pub platform_device: String,
    pub number: u8,
    pub min_start: u8,
    pub max_stop: u8,
}

/// braid-owned fan-control schema written by `modules/braid/cli.nix`; nested
/// config structs must enforce their own unknown-field rejection.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Module-owned auto-suspend config mirrored into the CLI so runtime
/// diagnostics can verify the same wake path NixOS configures at build time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSuspend {
    pub wol_interface: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    mount_point: MountPoint,
    pool_access_group: Option<String>,
    systemd_lifecycle: bool,
    fan_control: Option<FanControl>,
    ups: Option<Ups>,
    auto_suspend: Option<AutoSuspend>,
}

impl Config {
    pub fn new(mount_point: MountPoint) -> Result<Self, ConfigBuildError> {
        if mount_point.0.is_empty() {
            return Err(ConfigBuildError::EmptyMountPoint);
        }
        Ok(Config {
            mount_point,
            pool_access_group: None,
            systemd_lifecycle: false,
            fan_control: None,
            ups: None,
            auto_suspend: None,
        })
    }

    pub fn mount_point(&self) -> &MountPoint {
        &self.mount_point
    }

    /// Optional Unix group that receives write access on the mounted pool root
    /// via `root:<group> 2770`.
    pub fn pool_access_group(&self) -> Option<&str> {
        self.pool_access_group.as_deref()
    }

    /// True when module-generated config makes Rust responsible for
    /// `braid-online.service` and related systemd lifecycle synchronization.
    pub fn systemd_lifecycle(&self) -> bool {
        self.systemd_lifecycle
    }

    pub fn fan_control(&self) -> Option<&FanControl> {
        self.fan_control.as_ref()
    }

    pub fn ups(&self) -> Option<&Ups> {
        self.ups.as_ref()
    }

    /// Auto-suspend settings are presence-based so commands can skip WoL
    /// checks cleanly on hosts where suspend is not part of braid's contract.
    pub fn auto_suspend(&self) -> Option<&AutoSuspend> {
        self.auto_suspend.as_ref()
    }
}

/// Returns the mapper name for a disk: `braid-<name>`. Validated-type
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
#[serde(deny_unknown_fields)]
struct RawConfig {
    mount_point: MountPoint,
    #[serde(default)]
    pool_access_group: Option<String>,
    #[serde(default)]
    systemd_lifecycle: bool,
    #[serde(default)]
    fan_control: Option<FanControl>,
    #[serde(default)]
    ups: Option<Ups>,
    #[serde(default)]
    auto_suspend: Option<AutoSuspend>,
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigBuildError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let mut cfg = Config::new(raw.mount_point)?;
        cfg.pool_access_group = raw.pool_access_group;
        cfg.systemd_lifecycle = raw.systemd_lifecycle;
        cfg.fan_control = raw.fan_control;
        cfg.ups = raw.ups;
        cfg.auto_suspend = raw.auto_suspend;
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
        let raw = r#"{"mount_point":"/mnt/storage","pool_access_group":"storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.mount_point().as_str(), "/mnt/storage");
        assert_eq!(cfg.pool_access_group(), Some("storage"));
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

    // Intent: Config deserializes the auto_suspend block emitted by modules/braid/cli.nix.
    // Why: doctor needs the module-selected WoL interface at runtime; a schema
    // mismatch would silently skip the wake_on_lan check on auto-suspend hosts.
    // Scenario: NixOS generation with braid.autoSuspend.enable = true and
    // wolInterface = "eno1".
    #[test]
    fn parses_config_with_auto_suspend() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "auto_suspend": { "wol_interface": "eno1" }
        }"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        let auto = cfg.auto_suspend().expect("auto_suspend should be Some");
        assert_eq!(auto.wol_interface, "eno1");
    }

    // Intent: Config deserializes without systemd_lifecycle and defaults it to false.
    // Why it exists: standalone CLI configs omit module-owned lifecycle
    // capability and must not run braid-online systemctl calls.
    // Scenario: a CLI-only install writes only mount_point into config.json.
    #[test]
    fn parses_config_without_systemd_lifecycle_defaults_false() {
        let raw = r#"{"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert!(!cfg.systemd_lifecycle());
    }

    // Intent: Config deserializes explicit systemd_lifecycle=true.
    // Why it exists: modules/braid/cli.nix emits this field to opt Rust
    // dispatch into braid-online lifecycle ownership.
    // Scenario: module-managed install runs unlock/add/recover and expects
    // post-success lifecycle synchronization.
    #[test]
    fn parses_config_with_systemd_lifecycle_true() {
        let raw = r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert!(cfg.systemd_lifecycle());
    }

    // Intent: Config rejects non-boolean systemd_lifecycle values.
    // Why it exists: stringly capability flags would blur whether Rust owns
    // systemd lifecycle synchronization.
    // Scenario: operator hand-edits config.json and writes an invalid value.
    #[test]
    fn rejects_systemd_lifecycle_non_boolean() {
        let raw = r#"{"mount_point":"/mnt/storage","systemd_lifecycle":"yes"}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("non-boolean flag must fail");
        assert!(
            err.to_string().contains("boolean"),
            "expected boolean error, got: {err}"
        );
    }

    // Intent: Config rejects stale top-level field names after the rename.
    // Why it exists: accepting storage_group would silently skip the intended
    // pool_access_group setting and hide operator config drift.
    // Scenario: standalone config.json keeps the old key after an upgrade.
    #[test]
    fn rejects_config_with_unknown_top_level_field() {
        let raw = r#"{"mount_point":"/mnt/storage","storage_group":"storage"}"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("unknown field must fail");
        let message = err.to_string();
        assert!(
            message.contains("storage_group") || message.contains("unknown field"),
            "expected unknown-field error, got: {message}"
        );
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

    // Intent: Config parses when auto_suspend key is absent.
    // Why: hosts without braid.autoSuspend enabled must not fail config parsing
    // or run Wake-on-LAN-specific diagnostics.
    // Scenario: regular always-on NAS deployment runs any braid command.
    #[test]
    fn parses_config_without_auto_suspend() {
        let raw = r#"{"mount_point":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert!(cfg.auto_suspend().is_none());
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

    // Intent: unknown keys inside fan_control fail to deserialize.
    // Why it exists: RawConfig's deny_unknown_fields does not propagate into
    //   nested structs, so this pins the fan-control boundary explicitly.
    // Scenario: a future modules/braid/cli.nix adds a fan_control key against
    //   a CLI binary that predates the addition.
    #[test]
    fn rejects_unknown_field_in_fan_control() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "fan_control": {
                "pwm": {
                    "platform_device": "f71882fg.656",
                    "number": 1,
                    "min_start": 20,
                    "max_stop": 10
                },
                "min_temp": 30,
                "max_temp": 40,
                "min_fan_speed_percent": 20,
                "future_key": 1
            }
        }"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("unknown key must fail");
        let message = err.to_string();
        assert!(
            message.contains("future_key") || message.contains("unknown field"),
            "expected unknown-field error, got: {message}"
        );
    }

    // Intent: unknown keys inside auto_suspend fail to deserialize.
    // Why it exists: RawConfig's deny_unknown_fields does not propagate into
    // nested structs, so this pins the auto-suspend boundary explicitly.
    // Scenario: a future modules/braid/cli.nix adds an auto_suspend key against
    // a CLI binary that predates the addition.
    #[test]
    fn rejects_unknown_field_in_auto_suspend() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "auto_suspend": {
                "wol_interface": "eno1",
                "future_key": true
            }
        }"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("unknown key must fail");
        let message = err.to_string();
        assert!(
            message.contains("future_key") || message.contains("unknown field"),
            "expected unknown-field error, got: {message}"
        );
    }

    // Intent: unknown keys inside the nested pwm object fail to deserialize.
    // Why it exists: RawConfig's deny_unknown_fields does not propagate into
    //   nested structs, so this pins the pwm boundary explicitly.
    // Scenario: a future modules/braid/cli.nix adds a pwm key against a CLI
    //   binary that predates the addition.
    #[test]
    fn rejects_unknown_field_in_pwm() {
        let raw = r#"{
            "mount_point": "/mnt/storage",
            "fan_control": {
                "pwm": {
                    "platform_device": "f71882fg.656",
                    "number": 1,
                    "min_start": 20,
                    "max_stop": 10,
                    "future_key": 1
                },
                "min_temp": 30,
                "max_temp": 40,
                "min_fan_speed_percent": 20
            }
        }"#;
        let err = serde_json::from_str::<Config>(raw).expect_err("unknown key must fail");
        let message = err.to_string();
        assert!(
            message.contains("future_key") || message.contains("unknown field"),
            "expected unknown-field error, got: {message}"
        );
    }
}
