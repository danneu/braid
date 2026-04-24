use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{FindmntEntry, FindmntOutput};

// --- Serde helper structs (not exposed to domain code) ---

#[derive(Deserialize)]
struct RawFindmntOutput {
    filesystems: Vec<RawFindmntEntry>,
}

#[derive(Deserialize)]
struct RawFindmntEntry {
    target: String,
    source: String,
    fstype: String,
    #[serde(default)]
    options: String,
}

// --- Public parse function ---

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
                options: e.options,
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
    fn findmnt_parses_nixos_25_11_btrfs() {
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

    // --- Synthetic tests (inline) ---

    #[test]
    fn findmnt_returns_empty_inline() {
        let raw = RawCommandOutput {
            cmd: "findmnt".into(),
            stdout: r#"{"filesystems": []}"#.into(),
            stderr: String::new(),
            exit_status: 0,
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
}
