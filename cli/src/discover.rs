use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_cryptsetup_luks_label;
use crate::types::ByIdPath;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("failed to read /dev/disk/by-id: {0}")]
    ReadDir(#[source] std::io::Error),
}

/// Scan /dev/disk/by-id/ for LUKS devices with braid-<name> labels.
/// Returns a map of discovered pool members: name -> by_id path.
pub fn discover_pool_members<R: CommandRunner>(
    runner: &R,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    let by_id_dir = std::path::Path::new("/dev/disk/by-id");
    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(e) => return Err(DiscoverError::ReadDir(e)),
    };

    let mut members = BTreeMap::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip partition entries (e.g., ata-TOSHIBA-part1)
        if is_partition_entry(&name_str) {
            continue;
        }

        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();

        // Check if LUKS
        if runner
            .run(&CmdRequest::CryptsetupIsLuks {
                device: path_str.clone(),
            })
            .is_err()
        {
            continue;
        }

        // Read LUKS label via luksDump text output
        let label = match runner.run(&CmdRequest::CryptsetupLuksDumpText {
            device: path_str.clone(),
        }) {
            Ok(raw) => parse_cryptsetup_luks_label(&raw)
                .ok()
                .and_then(|out| out.label),
            Err(_) => continue,
        };

        // Check if label matches braid-<name>
        if let Some(label) = label {
            if let Some(disk_name) = crate::config::name_from_mapper(&label) {
                if crate::membership::is_valid_disk_name(disk_name) {
                    members.insert(disk_name.to_owned(), ByIdPath(path_str));
                }
            }
        }
    }

    Ok(members)
}

fn is_partition_entry(name: &str) -> bool {
    // Match -part1, -part2, etc. at end of name
    if let Some(idx) = name.rfind("-part") {
        let rest = &name[idx + 5..];
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_detection() {
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part1"));
        assert!(is_partition_entry("ata-TOSHIBA_MN08-part12"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-part"));
        assert!(!is_partition_entry("ata-TOSHIBA_MN08-partial"));
    }
}
