use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::types::{BtrfsDfEntry, BtrfsDfOutput};
use super::ParseError;

// --- Serde helper structs (not exposed to domain code) ---

// deny_unknown_fields: btrfs filesystem df --format json outputs its full schema;
// new fields signal a tool version change that needs parser investigation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBtrfsDfOutput {
    #[serde(rename = "filesystem-df")]
    filesystem_df: Vec<RawBtrfsDfEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBtrfsDfEntry {
    bg_type: String,
    bg_profile: String,
    bg_used: u64,
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
            "{}/tests/fixtures/phase2/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn btrfs_df_parses_raid1_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: fixture("btrfs-df-raid1.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_df_json(&raw).unwrap();
        assert_eq!(out.entries.len(), 3);
        assert_eq!(out.entries[0].bg_type, "Data");
        assert_eq!(out.entries[0].bg_profile, "RAID1");
        assert_eq!(out.entries[0].bg_used, 4294967296);
        assert_eq!(out.entries[0].bg_total, 5368709120);
    }

    #[test]
    fn btrfs_df_parses_single_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: fixture("btrfs-df-single.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_df_json(&raw).unwrap();
        assert_eq!(out.entries.len(), 3);
        assert_eq!(out.entries[0].bg_profile, "single");
    }

    #[test]
    fn btrfs_df_rejects_bad_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs filesystem df".into(),
            stdout: fixture("btrfs-df-bad.json"),
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
}
