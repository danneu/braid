use std::io::BufRead;

use crate::cmd::{CmdRequest, CommandRunner, LsblkFieldKind};
use crate::parse::parse_lsblk_field;

// ---------------------------------------------------------------------------
// format_bytes
// ---------------------------------------------------------------------------

pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// lsblk field helpers
// ---------------------------------------------------------------------------

pub fn get_lsblk_field<R: CommandRunner>(
    runner: &R,
    device: &str,
    field: LsblkFieldKind,
) -> Option<String> {
    let raw = runner
        .run(&CmdRequest::LsblkField {
            device: device.to_owned(),
            field,
        })
        .ok()?;
    parse_lsblk_field(&raw).ok()?.value
}

// ---------------------------------------------------------------------------
// DiskHwInfo
// ---------------------------------------------------------------------------

/// Hardware details for a disk, queried via lsblk.
/// All fields are optional because lsblk may fail (missing disk, permission
/// error, etc.).
#[derive(Debug, Clone, Default)]
pub struct DiskHwInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size: Option<u64>,
}

/// Query model, serial, and size for a device path via lsblk.
/// Returns None values gracefully for missing/dead disks.
pub fn query_disk_hw_info<R: CommandRunner>(runner: &R, device: &str) -> DiskHwInfo {
    let model = get_lsblk_field(runner, device, LsblkFieldKind::Model);
    let serial = get_lsblk_field(runner, device, LsblkFieldKind::Serial);
    let size_str = get_lsblk_field(runner, device, LsblkFieldKind::Size);
    let size = size_str.and_then(|s| s.parse::<u64>().ok());
    DiskHwInfo {
        model,
        serial,
        size,
    }
}

/// Format hardware info as a single line: "Model · 12.00 TiB · serial ABCD".
/// Returns None if no hardware info is available.
pub fn format_hw_info_line(info: &DiskHwInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(m) = &info.model {
        parts.push(m.clone());
    }
    if let Some(sz) = info.size {
        parts.push(format_bytes(sz));
    }
    if let Some(s) = &info.serial {
        parts.push(format!("serial {s}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" \u{00b7} ")) // middle dot separator
    }
}

// ---------------------------------------------------------------------------
// confirm_yes
// ---------------------------------------------------------------------------

/// Read a line and check it equals "yes".
/// Accepts a reader for testability.
pub fn confirm_yes_from<R: BufRead>(reader: &mut R) -> Result<(), String> {
    eprint!("Type 'yes' to continue: ");
    let mut input = String::new();
    reader
        .read_line(&mut input)
        .map_err(|e| format!("failed to read confirmation: {e}"))?;
    if input.trim() == "yes" {
        Ok(())
    } else {
        Err("aborted by user".into())
    }
}

/// Interactive confirmation: read "yes" from stdin.
pub fn confirm_yes() -> Result<(), String> {
    let mut stdin = std::io::stdin().lock();
    confirm_yes_from(&mut stdin)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- format_bytes ---

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1048576), "1.00 MiB");
        assert_eq!(format_bytes(1073741824), "1.00 GiB");
        assert_eq!(format_bytes(1099511627776), "1.00 TiB");
    }

    // --- format_hw_info_line ---

    #[test]
    fn hw_info_all_fields() {
        let info = DiskHwInfo {
            model: Some("Toshiba MN07ACA12T".into()),
            serial: Some("1234ABCD".into()),
            size: Some(12_000_138_625_024),
        };
        let line = format_hw_info_line(&info).unwrap();
        assert!(line.contains("Toshiba MN07ACA12T"));
        assert!(line.contains("TiB"));
        assert!(line.contains("serial 1234ABCD"));
        assert!(line.contains("\u{00b7}")); // middle dot separator
    }

    #[test]
    fn hw_info_partial_fields() {
        let info = DiskHwInfo {
            model: Some("Toshiba".into()),
            serial: None,
            size: None,
        };
        let line = format_hw_info_line(&info).unwrap();
        assert_eq!(line, "Toshiba");
    }

    #[test]
    fn hw_info_none_when_empty() {
        let info = DiskHwInfo::default();
        assert!(format_hw_info_line(&info).is_none());
    }

    // --- confirm_yes_from ---

    #[test]
    fn confirm_accepts_yes() {
        let mut input = std::io::Cursor::new(b"yes\n");
        assert!(confirm_yes_from(&mut input).is_ok());
    }

    #[test]
    fn confirm_rejects_no() {
        let mut input = std::io::Cursor::new(b"no\n");
        let err = confirm_yes_from(&mut input).unwrap_err();
        assert_eq!(err, "aborted by user");
    }

    #[test]
    fn confirm_rejects_empty() {
        let mut input = std::io::Cursor::new(b"\n");
        let err = confirm_yes_from(&mut input).unwrap_err();
        assert_eq!(err, "aborted by user");
    }

    #[test]
    fn confirm_trims_whitespace() {
        let mut input = std::io::Cursor::new(b"  yes  \n");
        assert!(confirm_yes_from(&mut input).is_ok());
    }
}
