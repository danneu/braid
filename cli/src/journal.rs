use std::path::Path;

use regex::Regex;

use crate::alert::AlertCause;
use crate::state_io::atomic_write;

pub const CURSOR_FILE: &str = "/var/lib/braid/journal-cursor";

const MAX_MESSAGE_LEN: usize = 200;

pub struct JournalCheckResult {
    pub causes: Vec<AlertCause>,
    pub new_cursor: Option<String>,
}

/// Scan the kernel journal for btrfs error entries since the last cursor.
///
/// Returns any `KernelJournalError` causes found and the cursor of the last
/// entry processed (matching or not). If no cursor file exists (first run),
/// initializes the cursor to the journal tail and returns no causes — this
/// avoids surfacing historical boot noise as new alerts.
pub fn check_journal(cursor_path: &Path) -> JournalCheckResult {
    let cursor = load_cursor(cursor_path);

    // First run: bootstrap cursor to now, return no causes.
    if cursor.is_none() {
        if let Err(e) = advance_cursor_to_now(cursor_path) {
            eprintln!("Warning: failed to initialize journal cursor: {e}");
        }
        return JournalCheckResult {
            causes: vec![],
            new_cursor: None,
        };
    }
    let cursor = cursor.unwrap();

    let output = match run_journalctl_after_cursor(&cursor) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("Warning: journalctl failed: {e}");
            return JournalCheckResult {
                causes: vec![],
                new_cursor: None,
            };
        }
    };

    parse_journal_output(&output)
}

/// Parse raw journalctl JSON-lines output into causes and a new cursor.
pub fn parse_journal_output(output: &str) -> JournalCheckResult {
    let mut last_cursor: Option<String> = None;
    let mut causes: Vec<AlertCause> = Vec::new();
    // Track which disk_names we've already added (for dedup).
    let mut seen_disk_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let re = Regex::new(r"/dev/mapper/braid-([a-zA-Z0-9_-]+)").unwrap();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let cursor = entry
            .get("__CURSOR")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        if let Some(ref c) = cursor {
            last_cursor = Some(c.clone());
        }

        let message = match entry.get("MESSAGE").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => continue,
        };

        if !message.contains("BTRFS error") {
            continue;
        }

        // Extract disk_name from MESSAGE, _UDEV_DEVNODE, _UDEV_DEVLINK
        let disk_name = extract_disk_name(&re, &entry);
        let truncated_message = truncate_message(message);

        match &disk_name {
            Some(name) => {
                // Deduplicate: one cause per disk_name
                if seen_disk_names.insert(name.clone()) {
                    causes.push(AlertCause::KernelJournalError {
                        message: truncated_message,
                        cursor: cursor.unwrap_or_default(),
                        disk_name: Some(name.clone()),
                    });
                }
            }
            None => {
                // Anonymous entries: keep each one (keyed by cursor)
                causes.push(AlertCause::KernelJournalError {
                    message: truncated_message,
                    cursor: cursor.unwrap_or_default(),
                    disk_name: None,
                });
            }
        }
    }

    JournalCheckResult {
        causes,
        new_cursor: last_cursor,
    }
}

/// Extract a braid disk name from journal entry fields.
fn extract_disk_name(re: &Regex, entry: &serde_json::Value) -> Option<String> {
    // Check MESSAGE first
    if let Some(message) = entry.get("MESSAGE").and_then(|v| v.as_str()) {
        if let Some(caps) = re.captures(message) {
            return Some(caps[1].to_owned());
        }
    }

    // Check _UDEV_DEVNODE
    if let Some(devnode) = entry.get("_UDEV_DEVNODE").and_then(|v| v.as_str()) {
        if let Some(caps) = re.captures(devnode) {
            return Some(caps[1].to_owned());
        }
    }

    // Check _UDEV_DEVLINK
    if let Some(devlink) = entry.get("_UDEV_DEVLINK").and_then(|v| v.as_str()) {
        if let Some(caps) = re.captures(devlink) {
            return Some(caps[1].to_owned());
        }
    }

    None
}

/// Truncate a message at a safe UTF-8 char boundary.
fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_MESSAGE_LEN {
        return message.to_owned();
    }
    let mut end = MAX_MESSAGE_LEN;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

fn run_journalctl_after_cursor(cursor: &str) -> Result<String, std::io::Error> {
    let output = std::process::Command::new("journalctl")
        .args([
            "-k",
            "-o",
            "json",
            "--no-pager",
            &format!("--after-cursor={cursor}"),
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "journalctl exited {}: {stderr}",
            output.status
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Cursor persistence
// ---------------------------------------------------------------------------

pub fn load_cursor(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

pub fn save_cursor(path: &Path, cursor: &str) -> Result<(), std::io::Error> {
    atomic_write(path, cursor.as_bytes())
}

/// Advance the cursor to the current journal tail. Used by `braid ack` and
/// first-run bootstrap.
pub fn advance_cursor_to_now(path: &Path) -> Result<(), std::io::Error> {
    let output = std::process::Command::new("journalctl")
        .args([
            "-k",
            "-o",
            "json",
            "--no-pager",
            "-n",
            "1",
            "--output-fields=__CURSOR",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "journalctl exited {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(cursor) = entry.get("__CURSOR").and_then(|v| v.as_str()) {
                return save_cursor(path, cursor);
            }
        }
    }

    // No journal entries at all — nothing to save, that's fine.
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(message: &str, cursor: &str) -> String {
        serde_json::json!({
            "MESSAGE": message,
            "__CURSOR": cursor,
        })
        .to_string()
    }

    fn make_entry_with_devnode(message: &str, cursor: &str, devnode: &str) -> String {
        serde_json::json!({
            "MESSAGE": message,
            "__CURSOR": cursor,
            "_UDEV_DEVNODE": devnode,
        })
        .to_string()
    }

    #[test]
    fn parse_matching_btrfs_error_entries() {
        let output = [
            make_entry(
                "BTRFS error (device dm-0): bdev /dev/mapper/braid-toshiba errs: wr 1",
                "cursor1",
            ),
            make_entry("BTRFS error (device dm-1): some other error", "cursor2"),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 2);
        assert_eq!(result.new_cursor, Some("cursor2".to_owned()));
    }

    #[test]
    fn non_matching_entries_skipped() {
        let output = [
            make_entry("some unrelated kernel message", "cursor1"),
            make_entry("another boring log line", "cursor2"),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        assert!(result.causes.is_empty());
        assert_eq!(result.new_cursor, Some("cursor2".to_owned()));
    }

    #[test]
    fn cursor_from_last_entry() {
        let output = [
            make_entry("unrelated", "cursor1"),
            make_entry("BTRFS error blah", "cursor2"),
            make_entry("also unrelated", "cursor3"),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        assert_eq!(result.new_cursor, Some("cursor3".to_owned()));
    }

    #[test]
    fn empty_output_no_causes_no_cursor() {
        let result = parse_journal_output("");
        assert!(result.causes.is_empty());
        assert!(result.new_cursor.is_none());
    }

    #[test]
    fn message_truncation_at_200_chars() {
        let long_msg = format!("BTRFS error {}", "x".repeat(300));
        let output = make_entry(&long_msg, "cursor1");

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 1);
        if let AlertCause::KernelJournalError { message, .. } = &result.causes[0] {
            assert!(message.len() <= MAX_MESSAGE_LEN);
        } else {
            panic!("Expected KernelJournalError");
        }
    }

    #[test]
    fn disk_name_extraction_from_message() {
        let output = make_entry(
            "BTRFS error (device dm-0): bdev /dev/mapper/braid-toshiba errs: wr 1",
            "cursor1",
        );

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 1);
        if let AlertCause::KernelJournalError { disk_name, .. } = &result.causes[0] {
            assert_eq!(disk_name.as_deref(), Some("toshiba"));
        } else {
            panic!("Expected KernelJournalError");
        }
    }

    #[test]
    fn disk_name_extraction_from_devnode() {
        let output = make_entry_with_devnode(
            "BTRFS error something generic",
            "cursor1",
            "/dev/mapper/braid-ironwolf",
        );

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 1);
        if let AlertCause::KernelJournalError { disk_name, .. } = &result.causes[0] {
            assert_eq!(disk_name.as_deref(), Some("ironwolf"));
        } else {
            panic!("Expected KernelJournalError");
        }
    }

    #[test]
    fn no_braid_pattern_gives_none_disk_name() {
        let output = make_entry(
            "BTRFS error (device dm-0): something without mapper name",
            "cursor1",
        );

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 1);
        if let AlertCause::KernelJournalError {
            disk_name, cursor, ..
        } = &result.causes[0]
        {
            assert!(disk_name.is_none());
            assert_eq!(cursor, "cursor1");
        } else {
            panic!("Expected KernelJournalError");
        }
    }

    #[test]
    fn dedup_multiple_entries_same_disk() {
        let output = [
            make_entry(
                "BTRFS error bdev /dev/mapper/braid-toshiba first",
                "cursor1",
            ),
            make_entry(
                "BTRFS error bdev /dev/mapper/braid-toshiba second",
                "cursor2",
            ),
            make_entry(
                "BTRFS error bdev /dev/mapper/braid-toshiba third",
                "cursor3",
            ),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        // Should collapse to one cause for "toshiba"
        assert_eq!(result.causes.len(), 1);
        if let AlertCause::KernelJournalError {
            disk_name, message, ..
        } = &result.causes[0]
        {
            assert_eq!(disk_name.as_deref(), Some("toshiba"));
            // First matching message is kept
            assert!(message.contains("first"));
        } else {
            panic!("Expected KernelJournalError");
        }
    }

    #[test]
    fn different_disks_are_separate_causes() {
        let output = [
            make_entry("BTRFS error bdev /dev/mapper/braid-toshiba err", "cursor1"),
            make_entry("BTRFS error bdev /dev/mapper/braid-ironwolf err", "cursor2"),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 2);
        let names: Vec<Option<&str>> = result
            .causes
            .iter()
            .map(|c| match c {
                AlertCause::KernelJournalError { disk_name, .. } => disk_name.as_deref(),
                _ => None,
            })
            .collect();
        assert!(names.contains(&Some("toshiba")));
        assert!(names.contains(&Some("ironwolf")));
    }

    #[test]
    fn anonymous_entries_with_different_cursors_are_separate() {
        let output = [
            make_entry("BTRFS error generic error one", "cursor1"),
            make_entry("BTRFS error generic error two", "cursor2"),
        ]
        .join("\n");

        let result = parse_journal_output(&output);
        assert_eq!(result.causes.len(), 2);
        for cause in &result.causes {
            if let AlertCause::KernelJournalError { disk_name, .. } = cause {
                assert!(disk_name.is_none());
            } else {
                panic!("Expected KernelJournalError");
            }
        }
    }

    #[test]
    fn first_run_bootstrap_cursor_persistence() {
        // Simulate first-run: no cursor file
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("journal-cursor");

        assert!(load_cursor(&cursor_path).is_none());

        // Save and reload
        save_cursor(&cursor_path, "s=abc123").unwrap();
        assert_eq!(load_cursor(&cursor_path), Some("s=abc123".to_owned()));
    }

    #[test]
    fn load_cursor_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("journal-cursor");
        std::fs::write(&cursor_path, "").unwrap();
        assert!(load_cursor(&cursor_path).is_none());
    }

    #[test]
    fn load_cursor_whitespace_only_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("journal-cursor");
        std::fs::write(&cursor_path, "  \n  ").unwrap();
        assert!(load_cursor(&cursor_path).is_none());
    }
}
