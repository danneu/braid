use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsBgType, BtrfsDfEntry, BtrfsDfOutput, BtrfsProfile};

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

    let entries = parsed
        .filesystem_df
        .into_iter()
        .map(|e| {
            let bg_type = match e.bg_type.as_str() {
                "Data" => BtrfsBgType::Data,
                "Metadata" => BtrfsBgType::Metadata,
                "System" => BtrfsBgType::System,
                "GlobalReserve" => BtrfsBgType::GlobalReserve,
                other => {
                    return Err(ParseError::UnexpectedValue {
                        cmd: raw.cmd.clone(),
                        field: "bg-type".into(),
                        value: other.into(),
                    });
                }
            };
            let bg_profile = match e.bg_profile.as_str() {
                "single" => BtrfsProfile::Single,
                "DUP" => BtrfsProfile::Dup,
                "RAID0" => BtrfsProfile::Raid0,
                "RAID1" => BtrfsProfile::Raid1,
                "RAID1C3" => BtrfsProfile::Raid1c3,
                "RAID1C4" => BtrfsProfile::Raid1c4,
                "RAID5" => BtrfsProfile::Raid5,
                "RAID6" => BtrfsProfile::Raid6,
                "RAID10" => BtrfsProfile::Raid10,
                other => BtrfsProfile::Unknown(other.to_owned()),
            };
            Ok(BtrfsDfEntry {
                bg_type,
                bg_profile,
                bg_used: e.bg_used,
                bg_total: e.bg_total,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(BtrfsDfOutput { entries })
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
        assert_eq!(out.entries[0].bg_type, BtrfsBgType::Data);
        assert_eq!(out.entries[0].bg_profile, BtrfsProfile::Raid1);
        assert_eq!(out.entries[0].bg_used, 16777216);
        assert_eq!(out.entries[0].bg_total, 105644032);
        assert_eq!(out.entries[1].bg_type, BtrfsBgType::System);
        assert_eq!(out.entries[2].bg_type, BtrfsBgType::Metadata);
        assert_eq!(out.entries[3].bg_type, BtrfsBgType::GlobalReserve);
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
        assert_eq!(out.entries[0].bg_profile, BtrfsProfile::Single);
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
    fn profiles_for_single_entry() {
        let df = BtrfsDfOutput {
            entries: vec![BtrfsDfEntry {
                bg_type: BtrfsBgType::Data,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 100,
                bg_total: 200,
            }],
        };
        let profiles = df.profiles_for(BtrfsBgType::Data);
        assert_eq!(profiles.len(), 1);
        assert!(profiles.contains(&BtrfsProfile::Raid1));
    }

    #[test]
    fn profiles_for_mixed() {
        let df = BtrfsDfOutput {
            entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 100,
                    bg_total: 200,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Single,
                    bg_used: 50,
                    bg_total: 100,
                },
            ],
        };
        let profiles = df.profiles_for(BtrfsBgType::Data);
        assert_eq!(profiles.len(), 2);
        assert!(profiles.contains(&BtrfsProfile::Single));
        assert!(profiles.contains(&BtrfsProfile::Raid1));
    }

    #[test]
    fn profiles_for_no_entries() {
        let df = BtrfsDfOutput {
            entries: vec![BtrfsDfEntry {
                bg_type: BtrfsBgType::Metadata,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 100,
                bg_total: 200,
            }],
        };
        let profiles = df.profiles_for(BtrfsBgType::Data);
        assert!(profiles.is_empty());
    }

    #[test]
    fn profiles_for_metadata() {
        let df = BtrfsDfOutput {
            entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 100,
                    bg_total: 200,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Metadata,
                    bg_profile: BtrfsProfile::Dup,
                    bg_used: 50,
                    bg_total: 100,
                },
            ],
        };
        let profiles = df.profiles_for(BtrfsBgType::Metadata);
        assert_eq!(profiles.len(), 1);
        assert!(profiles.contains(&BtrfsProfile::Dup));
    }

    /// Intent: logical_used_bytes sums Data + Metadata + System and
    /// excludes GlobalReserve, matching the "how full is this
    /// filesystem" contract.
    ///
    /// Why it exists: prevents regression to the "aggregate raw Used /
    /// Data ratio" approach that conflates block group profiles and
    /// produces >100% usage in the TUI (the 112% pool usage bug).
    ///
    /// Scenario: a filled pool where btrfs has reserved a nonzero
    /// GlobalReserve. A forgotten filter would silently add the
    /// reserve into used, overcounting by the reserve size.
    #[test]
    fn logical_used_bytes_excludes_global_reserve() {
        let df = BtrfsDfOutput {
            entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 100,
                    bg_total: 200,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Metadata,
                    bg_profile: BtrfsProfile::Dup,
                    bg_used: 20,
                    bg_total: 40,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::System,
                    bg_profile: BtrfsProfile::Dup,
                    bg_used: 3,
                    bg_total: 10,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::GlobalReserve,
                    bg_profile: BtrfsProfile::Single,
                    bg_used: 999,
                    bg_total: 999,
                },
            ],
        };
        // 100 + 20 + 3 = 123; GlobalReserve's 999 must be excluded.
        // A forgotten filter would produce 1122.
        assert_eq!(df.logical_used_bytes(), 123);
    }

    #[test]
    fn profiles_for_deduplicates() {
        let df = BtrfsDfOutput {
            entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 100,
                    bg_total: 200,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 300,
                    bg_total: 400,
                },
            ],
        };
        let profiles = df.profiles_for(BtrfsBgType::Data);
        assert_eq!(profiles.len(), 1);
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
