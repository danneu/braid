use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsDfEntry, BtrfsDfOutput};

// --- Serde helper structs (not exposed to domain code) ---

// deny_unknown_fields: btrfs filesystem df --format json outputs its full schema;
// new fields signal a tool version change that needs parser investigation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBtrfsDfOutput {
    #[serde(rename = "__header")]
    _header: Option<serde_json::Value>,

    #[serde(rename = "filesystem-df")]
    filesystem_df: Vec<RawBtrfsDfEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBtrfsDfEntry {
    #[serde(rename = "bg-type")]
    bg_type: String,

    #[serde(rename = "bg-profile")]
    bg_profile: String,

    #[serde(rename = "used")]
    bg_used: u64,

    #[serde(rename = "total")]
    bg_total: u64,
}

// --- Public parse function ---

pub fn parse_btrfs_df_json(raw: &RawCommandOutput) -> Result<BtrfsDfOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let parsed: RawBtrfsDfOutput =
        serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: e.to_string(),
        })?;

    Ok(BtrfsDfOutput {
        entries: parsed
            .filesystem_df
            .into_iter()
            .map(|e| BtrfsDfEntry {
                bg_type: e.bg_type,
                bg_profile: e.bg_profile,
                bg_used: e.bg_used,
                bg_total: e.bg_total,
            })
            .collect(),
    })
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
    fn btrfs_df_parses_nixos_25_11_raid1() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: fixture("btrfs-df-raid1.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_df_json(&raw).unwrap();
        assert_eq!(out.entries.len(), 4);
        assert_eq!(out.entries[0].bg_type, "Data");
        assert_eq!(out.entries[0].bg_profile, "RAID1");
        assert_eq!(out.entries[0].bg_used, 16777216);
        assert_eq!(out.entries[0].bg_total, 67108864);
        assert_eq!(out.entries[1].bg_type, "System");
        assert_eq!(out.entries[2].bg_type, "Metadata");
        assert_eq!(out.entries[3].bg_type, "GlobalReserve");
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn btrfs_df_parses_single_profile_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "single", "total": 1073741824, "used": 536870912 },
    { "bg-type": "Metadata", "bg-profile": "single", "total": 268435456, "used": 65536 },
    { "bg-type": "System", "bg-profile": "single", "total": 4194304, "used": 16384 }
  ]
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_df_json(&raw).unwrap();
        assert_eq!(out.entries.len(), 3);
        assert_eq!(out.entries[0].bg_profile, "single");
    }

    #[test]
    fn btrfs_df_rejects_malformed_json() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: r#"{"filesystem-df": "not an array"}"#.into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_df_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }

    #[test]
    fn btrfs_df_rejects_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        };
        let err = parse_btrfs_df_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn btrfs_df_rejects_legacy_underscore_keys() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: r#"{
  "filesystem-df": [
    { "bg_type": "Data", "bg_profile": "RAID1", "bg_total": 67108864, "bg_used": 16777216 }
  ]
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_df_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }
}
