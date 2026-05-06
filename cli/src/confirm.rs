use std::io::Read;
use zeroize::Zeroizing;

use crate::cmd::{CmdRequest, CommandRunner, LsblkFieldKind};

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
    if raw.exit_status != 0 {
        return None;
    }
    let trimmed = raw.stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
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

/// Format hardware info as a single line: "Model | 12.00 TiB | serial ABCD".
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
        Some(parts.join(" | "))
    }
}

// ---------------------------------------------------------------------------
// confirm_yes
// ---------------------------------------------------------------------------

/// Maximum accepted confirmation bytes before the newline.
///
/// Confirmation text is normally just `yes`; the cap keeps accidental pasted
/// secrets or hostile pipes out of a growable heap allocation.
const CONFIRM_MAX_BYTES: usize = 256;

/// Read a confirmation line from an unbuffered reader and require `yes`.
///
/// This helper intentionally accepts `Read`, not `BufRead`, so confirmation
/// cannot pre-drain bytes needed by a later `--passphrase-stdin` read.
pub fn confirm_yes_from<R: Read + ?Sized>(reader: &mut R) -> Result<(), String> {
    eprint!("Type 'yes' to continue: ");
    let mut buf: Zeroizing<[u8; CONFIRM_MAX_BYTES]> = Zeroizing::new([0u8; CONFIRM_MAX_BYTES]);
    let mut len = 0usize;
    let mut byte: Zeroizing<[u8; 1]> = Zeroizing::new([0u8; 1]);
    loop {
        let n = reader
            .read(&mut *byte)
            .map_err(|e| format!("failed to read confirmation: {e}"))?;
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        if len >= CONFIRM_MAX_BYTES {
            return Err("aborted by user".into());
        }
        buf[len] = byte[0];
        len += 1;
    }
    let input = std::str::from_utf8(&buf[..len]).unwrap_or("").trim();
    if input.trim() == "yes" {
        Ok(())
    } else {
        Err("aborted by user".into())
    }
}

/// Interactive confirmation: read "yes" from stdin.
pub fn confirm_yes() -> Result<(), String> {
    use std::os::unix::io::FromRawFd;
    let mut stdin_file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    confirm_yes_from(&mut *stdin_file)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

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
        assert!(line.contains(" | "));
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

    // --- get_lsblk_field ---

    fn lsblk_field_runner(output: RawCommandOutput) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::LsblkField {
                device: "/dev/sda".into(),
                field: LsblkFieldKind::Model,
            },
            output,
        )
    }

    // Intent: single-field lsblk queries trim successful output.
    // Why it exists: callers display plain hardware labels and should not
    //   inherit lsblk padding or trailing newlines.
    // Scenario: lsblk prints a disk model with surrounding whitespace.
    #[test]
    fn lsblk_field_trim_returns_value() {
        let runner = lsblk_field_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  Samsung SSD 870  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        });
        let value = get_lsblk_field(&runner, "/dev/sda", LsblkFieldKind::Model);
        assert_eq!(value.as_deref(), Some("Samsung SSD 870"));
    }

    // Intent: whitespace-only lsblk output is treated as a missing field.
    // Why it exists: callers use `None` for unavailable model, serial, or size
    //   values instead of displaying blank metadata.
    // Scenario: lsblk succeeds but the requested disk field is empty.
    #[test]
    fn lsblk_field_whitespace_only_returns_none() {
        let runner = lsblk_field_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "  \n".into(),
            stderr: String::new(),
            exit_status: 0,
        });
        let value = get_lsblk_field(&runner, "/dev/sda", LsblkFieldKind::Model);
        assert_eq!(value, None);
    }

    // Intent: failed lsblk field queries collapse to `None`.
    // Why it exists: hardware metadata is best-effort, so missing disks and
    //   lsblk failures should not abort status or confirmation flows.
    // Scenario: lsblk rejects a path that is not a block device.
    #[test]
    fn lsblk_field_nonzero_exit_returns_none() {
        let runner = lsblk_field_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: String::new(),
            stderr: "not a block device".into(),
            exit_status: 32,
        });
        let value = get_lsblk_field(&runner, "/dev/sda", LsblkFieldKind::Model);
        assert_eq!(value, None);
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

    // Intent: overlong confirmation lines are rejected outright.
    // Why it exists: truncating and then trimming could accept crafted input
    //   whose visible prefix is "yes" but whose full line is not.
    // Scenario: hostile or accidentally-large input reaches the confirm prompt.
    #[test]
    fn confirm_rejects_overlong_line() {
        let line: Vec<u8> = std::iter::repeat(b' ')
            .take(CONFIRM_MAX_BYTES + 1)
            .chain([b'\n'])
            .collect();
        let mut input = std::io::Cursor::new(line);
        let err = confirm_yes_from(&mut input).unwrap_err();
        assert_eq!(err, "aborted by user");
    }

    // Intent: "yes" followed by enough garbage to cross the cap rejects.
    // Why it exists: this directly pins the old truncate-collision shape.
    // Scenario: crafted input tries to make the capped prefix trim to "yes".
    #[test]
    fn confirm_rejects_yes_with_trailing_garbage_past_cap() {
        let mut line: Vec<u8> = b"yes".to_vec();
        line.extend(std::iter::repeat(b' ').take(CONFIRM_MAX_BYTES));
        line.extend(b"no\n");
        let mut input = std::io::Cursor::new(line);
        let err = confirm_yes_from(&mut input).unwrap_err();
        assert_eq!(err, "aborted by user");
    }
}
