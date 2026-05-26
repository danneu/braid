use nom::{
    IResult,
    bytes::complete::{tag, take_till1, take_until},
    character::complete::{not_line_ending, space0, space1, u64 as parse_u64},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::helpers::{parse_ctime, parse_duration_hms};
use super::types::{
    BtrfsScrubStatusPerDeviceOutput, DeviceScrubEntry, DeviceScrubState, ScrubTimestamp,
};

const CMD: &str = "btrfs scrub status -d -R";

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

/// Parses: "Scrub device /dev/dm-0 (id 1) status" → (Some("/dev/dm-0"), 1)
///     or: "Scrub device  (id 2) history"          → (None, 2)
fn parse_device_header(input: &str) -> IResult<&str, (Option<&str>, u64)> {
    let (input, _) = tag("Scrub device ")(input)?;
    let (input, before_id) = take_until("(id ")(input)?;
    let (input, _) = tag("(id ")(input)?;
    let (input, devid) = parse_u64(input)?;
    let (input, _) = tag(")")(input)?;
    let (input, _) = not_line_ending(input)?;

    let path = before_id.trim();
    let path = if path.is_empty() { None } else { Some(path) };
    Ok((input, (path, devid)))
}

/// Parses: "Scrub started:    Thu Feb 26 13:08:51 2026" → "Thu Feb 26 13:08:51 2026"
fn parse_scrub_started(input: &str) -> IResult<&str, &str> {
    let (input, _) = tag("Scrub started:")(input)?;
    let (input, _) = space0(input)?;
    let (input, ts) = not_line_ending(input)?;
    Ok((input, ts.trim()))
}

/// Parses an indented key-value line: "    data_bytes_scrubbed: 541917184" → ("data_bytes_scrubbed", 541917184)
fn parse_kv_u64(input: &str) -> IResult<&str, (&str, u64)> {
    let (input, _) = space1(input)?;
    let (input, key) = take_till1(|c| c == ':')(input)?;
    let (input, _) = tag(":")(input)?;
    let (input, _) = space1(input)?;
    let (input, value) = parse_u64(input)?;
    Ok((input, (key.trim(), value)))
}

fn map_status(s: &str) -> DeviceScrubState {
    match s {
        "running" => DeviceScrubState::Running,
        "finished" => DeviceScrubState::Finished,
        "aborted" => DeviceScrubState::Aborted,
        "interrupted" => DeviceScrubState::Interrupted,
        other => DeviceScrubState::Unknown(other.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Partial device accumulator
// ---------------------------------------------------------------------------

struct PartialDevice {
    devid: u64,
    path: Option<String>,
    state: DeviceScrubState,
    started_at: Option<ScrubTimestamp>,
    duration_secs: u64,
    data_bytes_scrubbed: u64,
    tree_bytes_scrubbed: u64,
    read_errors: u64,
    csum_errors: u64,
    verify_errors: u64,
    uncorrectable_errors: u64,
    corrected_errors: u64,
    super_errors: u64,
    last_physical: u64,
}

impl PartialDevice {
    fn new(devid: u64, path: Option<String>) -> Self {
        Self {
            devid,
            path,
            state: DeviceScrubState::Unknown(String::new()),
            started_at: None,
            duration_secs: 0,
            data_bytes_scrubbed: 0,
            tree_bytes_scrubbed: 0,
            read_errors: 0,
            csum_errors: 0,
            verify_errors: 0,
            uncorrectable_errors: 0,
            corrected_errors: 0,
            super_errors: 0,
            last_physical: 0,
        }
    }

    fn finalize(self) -> DeviceScrubEntry {
        DeviceScrubEntry {
            devid: self.devid,
            path: self.path,
            state: self.state,
            started_at: self.started_at,
            duration_secs: self.duration_secs,
            data_bytes_scrubbed: self.data_bytes_scrubbed,
            tree_bytes_scrubbed: self.tree_bytes_scrubbed,
            read_errors: self.read_errors,
            csum_errors: self.csum_errors,
            verify_errors: self.verify_errors,
            uncorrectable_errors: self.uncorrectable_errors,
            corrected_errors: self.corrected_errors,
            super_errors: self.super_errors,
            last_physical: self.last_physical,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_btrfs_scrub_status_per_device(
    raw: &RawCommandOutput,
) -> Result<BtrfsScrubStatusPerDeviceOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    // Extract UUID
    let uuid = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("UUID:").map(|v| v.trim().to_owned()))
        .ok_or_else(|| ParseError::MissingField {
            cmd: CMD.to_owned(),
            field: "UUID".to_owned(),
        })?;

    let mut devices: Vec<DeviceScrubEntry> = Vec::new();
    let mut current: Option<PartialDevice> = None;

    for line in stdout.lines() {
        // Try device header
        if let Ok((_, (path, devid))) = parse_device_header(line.trim()) {
            if let Some(partial) = current.take() {
                devices.push(partial.finalize());
            }
            current = Some(PartialDevice::new(devid, path.map(|s| s.to_owned())));
            continue;
        }

        let Some(ref mut dev) = current else {
            continue;
        };

        let trimmed = line.trim();

        // Status line
        if let Some(rest) = trimmed.strip_prefix("Status:") {
            dev.state = map_status(rest.trim());
            continue;
        }

        // Scrub started line
        if let Ok((_, ts)) = parse_scrub_started(trimmed) {
            if !ts.is_empty() {
                dev.started_at = parse_ctime(ts).map(ScrubTimestamp);
            }
            continue;
        }

        // Duration line
        if let Some(rest) = trimmed.strip_prefix("Duration:") {
            dev.duration_secs = parse_duration_hms(rest.trim()).unwrap_or(0);
            continue;
        }

        // Indented key-value counters
        if let Ok((_, (key, value))) = parse_kv_u64(line) {
            match key {
                "data_bytes_scrubbed" => dev.data_bytes_scrubbed = value,
                "tree_bytes_scrubbed" => dev.tree_bytes_scrubbed = value,
                "read_errors" => dev.read_errors = value,
                "csum_errors" => dev.csum_errors = value,
                "verify_errors" => dev.verify_errors = value,
                "uncorrectable_errors" => dev.uncorrectable_errors = value,
                "corrected_errors" => dev.corrected_errors = value,
                "super_errors" => dev.super_errors = value,
                "last_physical" => dev.last_physical = value,
                _ => {} // unknown keys silently ignored
            }
            continue;
        }
    }

    // Finalize last device
    if let Some(partial) = current.take() {
        devices.push(partial.finalize());
    }

    Ok(BtrfsScrubStatusPerDeviceOutput { uuid, devices })
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

    /// Intent: parse a real 3-device mid-scrub output.
    /// Why: ensures the parser handles the -d -R format from a live NixOS system.
    /// Scenario: user triggers scrub on a 3-drive pool; btrfs reports all devices as "running".
    #[test]
    fn parses_running_fixture() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: fixture("btrfs-scrub-per-device-running.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert!(
            uuid::Uuid::parse_str(&out.uuid).is_ok(),
            "UUID should be valid, got: {}",
            out.uuid
        );
        assert!(out.devices.len() >= 2, "expected at least 2 devices");
        for dev in &out.devices {
            assert!(dev.devid > 0, "devid should be positive");
            assert_eq!(dev.state, DeviceScrubState::Running);
            assert!(
                dev.data_bytes_scrubbed > 0 || dev.duration_secs == 0,
                "running device should have scrubbed bytes or zero duration"
            );
        }
    }

    /// Intent: parse a real 3-device post-scrub output.
    /// Why: ensures finished state mapping and path parsing.
    /// Scenario: scrub completes on a healthy pool; all drives finish.
    #[test]
    fn parses_finished_fixture() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: fixture("btrfs-scrub-per-device-finished.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert!(
            uuid::Uuid::parse_str(&out.uuid).is_ok(),
            "UUID should be valid, got: {}",
            out.uuid
        );
        assert!(out.devices.len() >= 2, "expected at least 2 devices");
        for dev in &out.devices {
            assert!(dev.devid > 0, "devid should be positive");
            assert!(
                matches!(
                    dev.state,
                    DeviceScrubState::Finished | DeviceScrubState::Aborted
                ),
                "expected Finished or Aborted, got {:?}",
                dev.state
            );
        }
        // At least one device should have scrubbed data
        assert!(
            out.devices.iter().any(|d| d.data_bytes_scrubbed > 0),
            "at least one device should have scrubbed bytes"
        );
    }

    // --- Synthetic tests (inline) ---

    /// Intent: minimal happy path for a single finished device.
    /// Why: validates the parser works with the simplest possible input.
    /// Scenario: single-disk pool after a clean scrub.
    #[test]
    fn single_device_finished() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           finished
Duration:         0:01:00
    data_bytes_scrubbed: 1000
    tree_bytes_scrubbed: 200
    read_errors: 0
    csum_errors: 0
    verify_errors: 0
    uncorrectable_errors: 0
    corrected_errors: 0
    super_errors: 0
    last_physical: 5000
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(out.uuid, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].devid, 1);
        assert_eq!(out.devices[0].path.as_deref(), Some("/dev/sda"));
        assert_eq!(out.devices[0].state, DeviceScrubState::Finished);
        assert_eq!(out.devices[0].duration_secs, 60);
        assert_eq!(out.devices[0].data_bytes_scrubbed, 1000);
        assert_eq!(out.devices[0].tree_bytes_scrubbed, 200);
        assert_eq!(out.devices[0].last_physical, 5000);
        assert_eq!(out.devices[0].total_errors(), 0);
    }

    /// Intent: interrupted per-device status maps to a typed state.
    /// Why: Keeps the TUI per-device path aligned with aggregate scrub status.
    /// Scenario: scrub userspace process exits before the kernel reports finish.
    #[test]
    fn single_device_interrupted() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           interrupted
Duration:         0:01:00
    data_bytes_scrubbed: 1000
    tree_bytes_scrubbed: 200
    read_errors: 0
    csum_errors: 0
    verify_errors: 0
    uncorrectable_errors: 0
    corrected_errors: 0
    super_errors: 0
    last_physical: 5000
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(out.devices[0].state, DeviceScrubState::Interrupted);
    }

    /// Intent: verify total_errors() sums correctly when errors are present.
    /// Why: the TUI will use total_errors() to flag problematic devices.
    /// Scenario: a device with read and csum errors after scrub.
    #[test]
    fn with_errors() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:30
    data_bytes_scrubbed: 500
    tree_bytes_scrubbed: 0
    read_errors: 3
    csum_errors: 1
    verify_errors: 0
    uncorrectable_errors: 0
    corrected_errors: 0
    super_errors: 0
    last_physical: 1000
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(out.devices[0].read_errors, 3);
        assert_eq!(out.devices[0].csum_errors, 1);
        assert_eq!(out.devices[0].total_errors(), 4);
    }

    // Intent: total_errors saturates when per-device counters exceed u64.
    // Why it exists: per-device scrub counters are diagnostic external-tool
    // output, so huge values must still render as "too many" instead of
    // panicking or wrapping.
    // Scenario: corrupt per-device scrub status reports u64::MAX read errors
    // plus one csum error.
    #[test]
    fn total_errors_saturates_on_large_counters() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:30
    data_bytes_scrubbed: 500
    tree_bytes_scrubbed: 0
    read_errors: 18446744073709551615
    csum_errors: 1
    verify_errors: 0
    uncorrectable_errors: 0
    corrected_errors: 0
    super_errors: 0
    last_physical: 1000
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(out.devices[0].total_errors(), u64::MAX);
    }

    /// Intent: non-zero exit status returns CommandFailed.
    /// Why: protects against silently returning partial data on command failure.
    /// Scenario: btrfs scrub status fails (e.g. filesystem not mounted).
    #[test]
    fn command_failed() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        };
        let err = parse_btrfs_scrub_status_per_device(&raw).unwrap_err();
        assert!(matches!(
            err,
            ParseError::CommandFailed { exit_code: 1, .. }
        ));
    }

    /// Intent: unknown keys from future btrfs-progs are silently skipped.
    /// Why: forward-compatibility — new fields shouldn't break the parser.
    /// Scenario: btrfs-progs adds a new counter field we don't track.
    #[test]
    fn unknown_keys_ignored() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
    data_bytes_scrubbed: 100
    tree_bytes_scrubbed: 0
    read_errors: 0
    csum_errors: 0
    verify_errors: 0
    no_csum: 42
    csum_discards: 7
    super_errors: 0
    malloc_errors: 0
    uncorrectable_errors: 0
    unverified_errors: 0
    corrected_errors: 0
    future_field: 999
    last_physical: 200
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].data_bytes_scrubbed, 100);
        assert_eq!(out.devices[0].last_physical, 200);
    }

    /// Intent: verify H:M:S duration parsing.
    /// Why: the TUI needs numeric seconds, not raw strings.
    /// Scenario: a scrub that ran for 3 minutes 15 seconds.
    #[test]
    fn duration_parses_hms() {
        assert_eq!(parse_duration_hms("0:03:15"), Some(195));
        assert_eq!(parse_duration_hms("1:00:00"), Some(3600));
        assert_eq!(parse_duration_hms("0:00:00"), Some(0));
        assert_eq!(parse_duration_hms("bad"), None);
    }

    /// Intent: unknown status string maps to DeviceScrubState::Unknown.
    /// Why: forward-compatibility for new btrfs scrub states.
    /// Scenario: future btrfs-progs adds a "cancelled" status.
    #[test]
    fn unknown_status_maps_to_unknown() {
        let raw = RawCommandOutput {
            cmd: CMD.into(),
            stdout: "\
UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

Scrub device /dev/sda (id 1) history
Scrub started:    Wed Feb 25 10:00:00 2026
Status:           cancelled
Duration:         0:00:01
    data_bytes_scrubbed: 0
    tree_bytes_scrubbed: 0
    read_errors: 0
    csum_errors: 0
    verify_errors: 0
    uncorrectable_errors: 0
    corrected_errors: 0
    super_errors: 0
    last_physical: 0
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status_per_device(&raw).unwrap();
        assert_eq!(
            out.devices[0].state,
            DeviceScrubState::Unknown("cancelled".to_owned())
        );
    }
}
