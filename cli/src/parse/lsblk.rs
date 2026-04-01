use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{LsblkDevice, LsblkFieldOutput, LsblkOutput};

// --- Serde helper structs (not exposed to domain code) ---

#[derive(Deserialize)]
struct RawLsblkOutput {
    blockdevices: Vec<RawLsblkDevice>,
}

#[derive(Deserialize)]
struct RawLsblkDevice {
    name: String,
    #[serde(rename = "type")]
    device_type: String,
    size: Option<u64>,
    model: Option<String>,
    serial: Option<String>,
    uuid: Option<String>,
    #[serde(default)]
    rota: Option<bool>,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default)]
    children: Vec<RawLsblkDevice>,
}

fn convert_lsblk_device(raw: RawLsblkDevice) -> LsblkDevice {
    LsblkDevice {
        name: raw.name,
        device_type: raw.device_type,
        size: raw.size,
        model: raw.model,
        serial: raw.serial,
        uuid: raw.uuid,
        rota: raw.rota,
        tran: raw.tran,
        children: raw.children.into_iter().map(convert_lsblk_device).collect(),
    }
}

// --- Public parse functions ---

pub fn parse_lsblk_json(raw: &RawCommandOutput) -> Result<LsblkOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let parsed: RawLsblkOutput =
        serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: e.to_string(),
        })?;

    Ok(LsblkOutput {
        blockdevices: parsed
            .blockdevices
            .into_iter()
            .map(convert_lsblk_device)
            .collect(),
    })
}

pub fn parse_lsblk_field(raw: &RawCommandOutput) -> Result<LsblkFieldOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let trimmed = raw.stdout.trim();
    let value = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    };

    Ok(LsblkFieldOutput { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-25.11/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn lsblk_parses_nixos_25_11_2disk() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: fixture("lsblk-2disk.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_json(&raw).unwrap();
        assert_eq!(out.blockdevices.len(), 2);
        assert_eq!(out.blockdevices[0].name, "vdb");
        assert_eq!(out.blockdevices[0].device_type, "disk");
        assert_eq!(out.blockdevices[0].size, Some(1073741824));
        assert_eq!(out.blockdevices[0].children.len(), 1);
        assert_eq!(out.blockdevices[0].children[0].device_type, "crypt");
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn lsblk_rejects_malformed_inline() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: r#"{"blockdevices": [{"name": 42, "missing_fields": true}]}"#.into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_lsblk_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }

    #[test]
    fn lsblk_rejects_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: String::new(),
            stderr: "error".into(),
            exit_status: 1,
        };
        let err = parse_lsblk_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    // --- parse_lsblk_field ---

    #[test]
    fn lsblk_field_extracts_value() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  Samsung SSD 870  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_field(&raw).unwrap();
        assert_eq!(out.value.as_deref(), Some("Samsung SSD 870"));
    }

    #[test]
    fn lsblk_field_returns_none_for_empty() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_field(&raw).unwrap();
        assert_eq!(out.value, None);
    }

    #[test]
    fn lsblk_field_errors_on_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: String::new(),
            stderr: "not a block device".into(),
            exit_status: 32,
        };
        let err = parse_lsblk_field(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }
}
