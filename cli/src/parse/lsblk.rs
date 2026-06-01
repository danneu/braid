use serde::{Deserialize, Deserializer};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{LsblkDevice, LsblkOutput};

// --- Serde helper structs (not exposed to domain code) ---

#[derive(Deserialize)]
struct RawLsblkOutput {
    blockdevices: Vec<RawLsblkDevice>,
}

/// Enforces that always-requested nullable lsblk columns remain present.
/// Missing keys indicate command/output drift, while explicit null remains
/// valid for devices that do not have that attribute.
fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Deserialize)]
struct RawLsblkDevice {
    name: String,
    #[serde(rename = "type")]
    device_type: String,
    #[serde(deserialize_with = "required_option")]
    size: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    model: Option<String>,
    #[serde(deserialize_with = "required_option")]
    serial: Option<String>,
    #[serde(deserialize_with = "required_option")]
    uuid: Option<String>,
    #[serde(deserialize_with = "required_option")]
    rota: Option<bool>,
    #[serde(deserialize_with = "required_option")]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-26.05/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    fn raw_lsblk(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn assert_missing_required_key(stdout: &str) {
        let err = parse_lsblk_json(&raw_lsblk(stdout)).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }

    // --- Contract tests (nixos-26.05 fixtures) ---

    #[test]
    fn lsblk_parses_nixos_26_05_2disk() {
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
        let raw = raw_lsblk(r#"{"blockdevices": [{"name": 42, "missing_fields": true}]}"#);
        let err = parse_lsblk_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }

    // Intent: lsblk JSON must include the requested SIZE column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the size key.
    #[test]
    fn lsblk_rejects_missing_required_size_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","model":null,
                "serial":null,"uuid":null,"rota":true,"tran":"sata"
            }]}"#,
        );
    }

    // Intent: lsblk JSON must include the requested MODEL column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the model key.
    #[test]
    fn lsblk_rejects_missing_required_model_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","size":1,
                "serial":null,"uuid":null,"rota":true,"tran":"sata"
            }]}"#,
        );
    }

    // Intent: lsblk JSON must include the requested SERIAL column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the serial key.
    #[test]
    fn lsblk_rejects_missing_required_serial_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","size":1,"model":null,
                "uuid":null,"rota":true,"tran":"sata"
            }]}"#,
        );
    }

    // Intent: lsblk JSON must include the requested UUID column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the uuid key.
    #[test]
    fn lsblk_rejects_missing_required_uuid_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","size":1,"model":null,
                "serial":null,"rota":true,"tran":"sata"
            }]}"#,
        );
    }

    // Intent: lsblk JSON must include the requested ROTA column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the rota key.
    #[test]
    fn lsblk_rejects_missing_required_rota_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","size":1,"model":null,
                "serial":null,"uuid":null,"tran":"sata"
            }]}"#,
        );
    }

    // Intent: lsblk JSON must include the requested TRAN column.
    // Why it exists: serde Option<T> would otherwise turn a missing key
    // into None and hide command/output drift.
    // Scenario: upstream or capture drift omits only the tran key.
    #[test]
    fn lsblk_rejects_missing_required_tran_key() {
        assert_missing_required_key(
            r#"{"blockdevices":[{
                "name":"vdb","type":"disk","size":1,"model":null,
                "serial":null,"uuid":null,"rota":true
            }]}"#,
        );
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
}
