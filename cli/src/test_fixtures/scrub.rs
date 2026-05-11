//! Scrub-scope fixtures for the small scrub command test modules.
//!
//! The helpers stay flat and request-shaped so each test still composes only
//! the command probes it expects. There is no broad scrub runner: unexpected
//! cross-command probes continue to surface through `MockRunner` as missing
//! mocks.

use super::shared::mock_ok;
use crate::cmd::{CmdRequest, RawCommandOutput};
use crate::types::MountPoint;

/// Canonical scrub-test mount point shared by cancel, status, resume, and start fixtures.
pub(crate) fn scrub_mp() -> MountPoint {
    MountPoint("/mnt/storage".into())
}

/// Successful `btrfs scrub cancel` output for the running-scrub path.
pub(crate) fn scrub_cancel_ok() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubCancel {
            mount_point: scrub_mp(),
        },
        mock_ok("btrfs scrub cancel /mnt/storage", "scrub cancelled\n"),
    )
}

/// ENOTCONN-shaped `btrfs scrub cancel` output: exit 2 means no scrub was running.
pub(crate) fn scrub_cancel_not_running() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubCancel {
            mount_point: scrub_mp(),
        },
        RawCommandOutput {
            cmd: "btrfs scrub cancel /mnt/storage".into(),
            stdout: String::new(),
            stderr: "ERROR: scrub cancel failed on /mnt/storage: not running\n".into(),
            exit_status: 2,
        },
    )
}

/// Non-ENOTCONN `btrfs scrub cancel` output that must remain a real failure.
pub(crate) fn scrub_cancel_real_failure() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubCancel {
            mount_point: scrub_mp(),
        },
        RawCommandOutput {
            cmd: "btrfs scrub cancel /mnt/storage".into(),
            stdout: String::new(),
            stderr: "ERROR: scrub cancel failed on /mnt/storage: Permission denied\n".into(),
            exit_status: 1,
        },
    )
}

/// Running scrub-status output for tests that only need to classify the state.
pub(crate) fn scrub_status_running() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output(
        "UUID:             12345678-1234-1234-1234-123456789abc\n\
         Scrub started:    Mon Jan  1 00:00:00 2024\n\
         Status:           running\n\
         Duration:         0:00:05\n\
         Total to scrub:   30408704000\n\
         Rate:             2952790016/s\n\
         10.00% done\n\
         Error summary:    no errors found\n",
    )
}

/// Never-scrubbed status output using the real `--raw` no-stats framing.
pub(crate) fn scrub_status_never() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output(
        "UUID:             12345678-1234-1234-1234-123456789abc\n\
         \tno stats available\n\
         Total to scrub:   33914880\n\
         Rate:             0/s\n\
         Error summary:    no errors found\n",
    )
}

/// Finished scrub-status output for cleanly completed scrub progress.
pub(crate) fn scrub_status_finished() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output(
        "UUID:             12345678-1234-1234-1234-123456789abc\n\
         Scrub started:    Mon Jan  1 00:00:00 2024\n\
         Status:           finished\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Aborted scrub-status output for resumable progress left by cancellation.
pub(crate) fn scrub_status_aborted() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output(
        "UUID:             12345678-1234-1234-1234-123456789abc\n\
         Scrub started:    Mon Jan  1 00:00:00 2024\n\
         Status:           aborted\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Interrupted scrub-status output for resumable userspace scrub progress.
pub(crate) fn scrub_status_interrupted() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output(
        "UUID:             12345678-1234-1234-1234-123456789abc\n\
         Scrub started:    Mon Jan  1 00:00:00 2024\n\
         Status:           interrupted\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Empty successful scrub-status output that forces the parser into `Unknown`.
pub(crate) fn scrub_status_unknown() -> (CmdRequest, RawCommandOutput) {
    scrub_status_output("")
}

/// Parameterised `btrfs scrub resume -B` output keyed by btrfs exit code.
pub(crate) fn scrub_resume_output(exit_status: i32) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubResume {
            mount_point: scrub_mp(),
        },
        RawCommandOutput {
            cmd: "btrfs scrub resume -B /mnt/storage".into(),
            stdout: String::new(),
            stderr: if exit_status == 1 {
                "ERROR: resume failed\n".into()
            } else {
                String::new()
            },
            exit_status,
        },
    )
}

/// Parameterised `btrfs scrub start -B` output keyed by btrfs exit code.
pub(crate) fn scrub_start_output(exit_status: i32) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStart {
            mount_point: scrub_mp(),
        },
        RawCommandOutput {
            cmd: "btrfs scrub start -B /mnt/storage".into(),
            stdout: String::new(),
            stderr: if exit_status == 1 {
                "ERROR: start failed\n".into()
            } else {
                String::new()
            },
            exit_status,
        },
    )
}

fn scrub_status_output(stdout: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStatus {
            mount_point: scrub_mp(),
        },
        mock_ok("btrfs scrub status --raw /mnt/storage", stdout),
    )
}
