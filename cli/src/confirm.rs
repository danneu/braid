use std::io::Read;
use zeroize::Zeroizing;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_lsblk_json;

// ---------------------------------------------------------------------------
// Confirm seam
// ---------------------------------------------------------------------------

/// Seam for the operator go/no-go prompt used by mutating commands.
///
/// Production prints the already-assembled prompt and reads `yes` from the
/// real tty; tests record the prompt and return an armed verdict.
pub trait Confirm {
    /// Show `prompt` and require the operator to approve the operation.
    fn confirm(&self, prompt: &str) -> Result<(), String>;
}

/// Production confirmation that preserves the existing stdin behavior.
pub struct RealConfirm;

impl Confirm for RealConfirm {
    fn confirm(&self, prompt: &str) -> Result<(), String> {
        eprint!("{prompt}");
        confirm_yes()
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Verdict {
    #[default]
    Unexpected,
    Accept,
    Decline,
}

/// Test confirmation seam that records prompts and fails closed unless armed.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingConfirm {
    verdict: std::cell::Cell<Verdict>,
    prompts: std::cell::RefCell<Vec<String>>,
}

#[cfg(test)]
impl RecordingConfirm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&self) {
        self.verdict.set(Verdict::Accept);
    }

    pub fn decline(&self) {
        self.verdict.set(Verdict::Decline);
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.borrow().clone()
    }

    pub fn last_prompt(&self) -> Option<String> {
        self.prompts.borrow().last().cloned()
    }
}

#[cfg(test)]
impl Confirm for RecordingConfirm {
    fn confirm(&self, prompt: &str) -> Result<(), String> {
        self.prompts.borrow_mut().push(prompt.to_owned());
        match self.verdict.get() {
            Verdict::Unexpected => panic!("confirmation requested without an armed verdict"),
            Verdict::Accept => Ok(()),
            Verdict::Decline => Err("aborted by user".into()),
        }
    }
}

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
// DiskHwInfo
// ---------------------------------------------------------------------------

/// Hardware details for a disk, queried via lsblk.
/// All fields are optional because lsblk may fail (missing disk, permission
/// error, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskHwInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size: Option<u64>,
}

/// Query model, serial, and size for a device path via lsblk.
/// Returns None values gracefully for missing/dead disks.
pub fn query_disk_hw_info<R: CommandRunner>(runner: &R, device: &str) -> DiskHwInfo {
    let Some(raw) = runner
        .run(&CmdRequest::LsblkDeviceJson {
            device: device.to_owned(),
        })
        .ok()
    else {
        return DiskHwInfo::default();
    };
    let Some(mut devices) = parse_lsblk_json(&raw)
        .ok()
        .map(|output| output.blockdevices)
    else {
        return DiskHwInfo::default();
    };
    if devices.len() != 1 {
        return DiskHwInfo::default();
    }
    let parsed = devices.pop().expect("length checked above");
    let normalize = |value: Option<String>| {
        value.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
    };
    DiskHwInfo {
        model: normalize(parsed.model),
        serial: normalize(parsed.serial),
        size: parsed.size,
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
    // dup so we hand a plain File to confirm_yes_from -- std::io::stdin()
    // would re-engage Stdin's line buffer and pre-drain bytes the next
    // --passphrase-stdin reader needs.
    let stdin_fd = nix::unistd::dup(std::io::stdin()).map_err(|e| format!("dup stdin: {e}"))?;
    let mut stdin_file = std::fs::File::from(stdin_fd);
    confirm_yes_from(&mut stdin_file)
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

    // --- query_disk_hw_info ---

    fn lsblk_device_runner(output: RawCommandOutput) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::LsblkDeviceJson {
                device: "/dev/sda".into(),
            },
            output,
        )
    }

    fn lsblk_device_json(blockdevices: &str) -> String {
        format!(r#"{{"blockdevices":[{blockdevices}]}}"#)
    }

    fn lsblk_device(model: &str, serial: &str, size: &str) -> String {
        format!(
            r#"{{
                "name":"sda","type":"disk","size":{size},
                "model":{model},"serial":{serial},"uuid":null,
                "rota":true,"tran":"sata"
            }}"#,
        )
    }

    // Intent: one structured query returns and normalizes every hardware field.
    // Why it exists: callers display plain hardware labels and should not
    //   inherit lsblk whitespace or issue one subprocess per field.
    // Scenario: lsblk returns a padded model, blank serial, and numeric size.
    #[test]
    fn disk_hw_query_normalizes_one_structured_result() {
        let runner = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: lsblk_device_json(&lsblk_device(
                r#""  Samsung SSD 870  ""#,
                r#""   ""#,
                "1000000",
            )),
            stderr: String::new(),
            exit_status: 0,
        });
        let info = query_disk_hw_info(&runner, "/dev/sda");

        assert_eq!(
            info,
            DiskHwInfo {
                model: Some("Samsung SSD 870".into()),
                serial: None,
                size: Some(1_000_000),
            }
        );
        assert_eq!(runner.requests().len(), 1);
    }

    // Intent: explicit JSON nulls degrade fields independently.
    // Why it exists: real disks may omit model or serial without making SIZE
    //   unavailable to replacement preflight.
    // Scenario: lsblk reports only SIZE for a device.
    #[test]
    fn disk_hw_query_preserves_nullable_fields() {
        let runner = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: lsblk_device_json(&lsblk_device("null", "null", "42")),
            stderr: String::new(),
            exit_status: 0,
        });
        let info = query_disk_hw_info(&runner, "/dev/sda");

        assert_eq!(info.model, None);
        assert_eq!(info.serial, None);
        assert_eq!(info.size, Some(42));
    }

    // Intent: command and JSON failures collapse to unavailable metadata.
    // Why it exists: hardware metadata is best-effort, so missing disks and
    //   lsblk failures should not abort status or confirmation flows.
    // Scenario: lsblk rejects a path or returns malformed structured output.
    #[test]
    fn disk_hw_query_failures_return_empty_info() {
        let failed = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: String::new(),
            stderr: "not a block device".into(),
            exit_status: 32,
        });
        let malformed = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: "not json".into(),
            stderr: String::new(),
            exit_status: 0,
        });

        assert_eq!(
            query_disk_hw_info(&failed, "/dev/sda"),
            DiskHwInfo::default()
        );
        assert_eq!(
            query_disk_hw_info(&malformed, "/dev/sda"),
            DiskHwInfo::default()
        );
        assert_eq!(
            query_disk_hw_info(&MockRunner::default(), "/dev/sda"),
            DiskHwInfo::default()
        );
    }

    // Intent: a device-scoped query must identify exactly one device.
    // Why it exists: silently selecting one row could attach another disk's
    //   hardware to a destructive confirmation prompt.
    // Scenario: an unexpected lsblk result contains zero or two roots.
    #[test]
    fn disk_hw_query_rejects_ambiguous_result_shapes() {
        let empty = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: lsblk_device_json(""),
            stderr: String::new(),
            exit_status: 0,
        });
        let row = lsblk_device("null", "null", "42");
        let multiple = lsblk_device_runner(RawCommandOutput {
            cmd: "lsblk".into(),
            stdout: lsblk_device_json(&format!("{row},{row}")),
            stderr: String::new(),
            exit_status: 0,
        });

        assert_eq!(
            query_disk_hw_info(&empty, "/dev/sda"),
            DiskHwInfo::default()
        );
        assert_eq!(
            query_disk_hw_info(&multiple, "/dev/sda"),
            DiskHwInfo::default()
        );
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
        let line: Vec<u8> = std::iter::repeat_n(b' ', CONFIRM_MAX_BYTES + 1)
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
        line.extend(std::iter::repeat_n(b' ', CONFIRM_MAX_BYTES));
        line.extend(b"no\n");
        let mut input = std::io::Cursor::new(line);
        let err = confirm_yes_from(&mut input).unwrap_err();
        assert_eq!(err, "aborted by user");
    }

    // Intent: RecordingConfirm records the full prompt and accepts only after
    //   the test arms an accepting verdict.
    // Why it exists: command tests need to assert prompt bytes without reading
    //   from the real process stdin.
    // Scenario: a mutating command reaches its `yes=false` gate and the test
    //   allows it to proceed.
    #[test]
    fn recording_confirm_accept_records_prompt() {
        let confirm = RecordingConfirm::new();
        confirm.accept();

        confirm.confirm("proceed?\n").expect("armed accept");

        assert_eq!(confirm.prompts(), vec!["proceed?\n".to_owned()]);
        assert_eq!(confirm.last_prompt().as_deref(), Some("proceed?\n"));
    }

    // Intent: RecordingConfirm returns the same decline text as confirm_yes.
    // Why it exists: command tests should exercise the same error surface as
    //   the production prompt without needing a tty.
    // Scenario: a mutating command reaches its prompt and the operator does
    //   not type the exact approval word.
    #[test]
    fn recording_confirm_decline_matches_confirm_yes() {
        let confirm = RecordingConfirm::new();
        confirm.decline();

        let err = confirm.confirm("proceed?\n").unwrap_err();

        assert_eq!(err, "aborted by user");
        assert_eq!(confirm.prompts(), vec!["proceed?\n".to_owned()]);
    }

    // Intent: RecordingConfirm panics when production code prompts without a
    //   test arming the verdict.
    // Why it exists: fail-closed defaults catch regressions where `--yes`
    //   still asks for confirmation.
    // Scenario: a test fixture leaves the recorder unarmed because the path
    //   should not prompt, but the command unexpectedly reaches the gate.
    #[test]
    #[should_panic(expected = "confirmation requested without an armed verdict")]
    fn recording_confirm_unarmed_panics() {
        let confirm = RecordingConfirm::new();
        let _ = confirm.confirm("unexpected\n");
    }
}
