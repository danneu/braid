use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::types::{
    BtrfsDfEntry, BtrfsDfOutput, FindmntEntry, FindmntOutput, LsblkDevice, LsblkOutput,
};
use super::ParseError;

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
        children: raw.children.into_iter().map(convert_lsblk_device).collect(),
    }
}

#[derive(Deserialize)]
struct RawFindmntOutput {
    filesystems: Vec<RawFindmntEntry>,
}

#[derive(Deserialize)]
struct RawFindmntEntry {
    target: String,
    source: String,
    fstype: String,
}

#[derive(Deserialize)]
struct RawBtrfsDfOutput {
    #[serde(rename = "filesystem-df")]
    filesystem_df: Vec<RawBtrfsDfEntry>,
}

#[derive(Deserialize)]
struct RawBtrfsDfEntry {
    bg_type: String,
    bg_profile: String,
    bg_used: u64,
    bg_total: u64,
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

pub fn parse_findmnt_json(raw: &RawCommandOutput) -> Result<FindmntOutput, ParseError> {
    // findmnt exits non-zero when mount point is not found — benign ONLY if
    // stderr is empty (no unexpected error message). Any stderr content on
    // non-zero exit is treated as a real failure.
    if raw.exit_status != 0 {
        let stderr = raw.stderr.trim();
        if stderr.is_empty() {
            return Ok(FindmntOutput {
                filesystems: vec![],
            });
        }
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let parsed: RawFindmntOutput =
        serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: e.to_string(),
        })?;

    Ok(FindmntOutput {
        filesystems: parsed
            .filesystems
            .into_iter()
            .map(|e| FindmntEntry {
                target: e.target,
                source: e.source,
                fstype: e.fstype,
            })
            .collect(),
    })
}

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

    // --- parse_lsblk_json ---

    #[test]
    fn lsblk_parses_2disk_fixture() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: fixture("lsblk-2disk.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_lsblk_json(&raw).unwrap();
        assert_eq!(out.blockdevices.len(), 2);
        assert_eq!(out.blockdevices[0].name, "vda");
        assert_eq!(out.blockdevices[0].device_type, "disk");
        assert_eq!(out.blockdevices[0].size, Some(10737418240));
        assert_eq!(out.blockdevices[0].children.len(), 1);
        assert_eq!(out.blockdevices[0].children[0].device_type, "crypt");
    }

    #[test]
    fn lsblk_rejects_bad_json() {
        let raw = RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: fixture("lsblk-bad.json"),
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

    // --- parse_findmnt_json ---

    #[test]
    fn findmnt_parses_btrfs_mount() {
        let raw = RawCommandOutput {
            cmd: "findmnt".into(),
            stdout: fixture("findmnt-btrfs.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_findmnt_json(&raw).unwrap();
        assert_eq!(out.filesystems.len(), 1);
        assert_eq!(out.filesystems[0].target, "/mnt/storage");
        assert_eq!(out.filesystems[0].fstype, "btrfs");
    }

    #[test]
    fn findmnt_returns_empty_on_not_found() {
        let raw = RawCommandOutput {
            cmd: "findmnt".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 1,
        };
        let out = parse_findmnt_json(&raw).unwrap();
        assert!(out.filesystems.is_empty());
    }

    #[test]
    fn findmnt_errors_on_unexpected_stderr() {
        let raw = RawCommandOutput {
            cmd: "findmnt".into(),
            stdout: String::new(),
            stderr: "findmnt: unknown filesystem type 'zfs'".into(),
            exit_status: 1,
        };
        let err = parse_findmnt_json(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    // --- parse_btrfs_df_json ---

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
