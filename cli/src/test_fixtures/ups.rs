//! UPS-scope fixtures for `cli/src/ups.rs::tests`.
//!
//! The helpers stay request-shaped and config-shaped so the runner and
//! on-disk config boundary tests keep their exact contracts. There is no
//! broad UPS runner: the missing-mock tests deliberately use bare
//! `MockRunner::default()` so runner invocation failures stay observable.

use super::shared::mock_ok;
use crate::cmd::{CmdRequest, RawCommandOutput};
use std::path::PathBuf;
use tempfile::TempDir;

/// Write the `config.json` shape expected by `cmd_ups_status` tests.
pub(crate) fn ups_write_config(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        format!(r#"{{"mount_point":"/mnt/storage","ups":{{"name":"{name}"}}}}"#),
    )
    .unwrap();
    path
}

/// Minimal healthy `upsc` response that proves OL status and 100% charge parse.
pub(crate) fn ups_query_healthy_minimal() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::UpscQuery { name: "ups".into() },
        mock_ok("upsc ups", "ups.status: OL\nbattery.charge: 100\n"),
    )
}

/// Daemon-down `upsc` response with stderr newline for the trim-proof test.
pub(crate) fn ups_query_connection_refused_with_newline() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::UpscQuery { name: "ups".into() },
        RawCommandOutput {
            cmd: "upsc ups".into(),
            stdout: String::new(),
            stderr: "Error: Connection failure: Connection refused\n".into(),
            exit_status: 1,
        },
    )
}

/// Daemon-down `upsc` response without stderr newline for Display-format tests.
pub(crate) fn ups_query_connection_refused_no_newline() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::UpscQuery { name: "ups".into() },
        RawCommandOutput {
            cmd: "upsc ups".into(),
            stdout: String::new(),
            stderr: "Error: Connection failure: Connection refused".into(),
            exit_status: 1,
        },
    )
}

/// Non-zero `upsc` response with empty stderr for suffix-rendering tests.
pub(crate) fn ups_query_empty_stderr_exit_1() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::UpscQuery { name: "ups".into() },
        RawCommandOutput {
            cmd: "upsc ups".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 1,
        },
    )
}
