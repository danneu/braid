//! Doctor scope fixtures: cross-test scaffolding for `cli/src/doctor.rs`'s
//! `mod tests`.
//!
//! Doctor is check-oriented (read state, render result), not mutating-command
//! oriented, so this module is a flat collection of helpers rather than the
//! `*Pool` + `*ParamsBuilder` + `PoolFixture` triad the other commands use.
//!
//! Field-construction of `DoctorContext` and `BeepCheckOptions` happens via
//! the `#[cfg(test)] pub(crate)` constructors on those types in `doctor.rs`
//! (see the "Test-only constructors" section there); doctor.rs holds the
//! private fields, so this module cannot field-literal-construct them
//! directly.

use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::doctor::{BeepCheckOptions, DiskState, DoctorContext, DoctorOptions};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use std::io::Write;
use std::sync::Mutex;
use tempfile::{NamedTempFile, TempDir};

// ---------------------------------------------------------------------------
// Path / file primitives
// ---------------------------------------------------------------------------

pub(crate) fn isolated_paths() -> (TempDir, StatePaths) {
    let dir = TempDir::new().unwrap();
    let paths = StatePaths::custom(dir.path().to_owned());
    (dir, paths)
}

pub(crate) fn write_temp(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

// ---------------------------------------------------------------------------
// Default option builders
// ---------------------------------------------------------------------------

pub(crate) fn human_options() -> DoctorOptions {
    DoctorOptions {
        json: false,
        beep: false,
    }
}

pub(crate) fn beep_check_options(
    is_root: bool,
    json_output: bool,
    play_beep: bool,
) -> BeepCheckOptions {
    BeepCheckOptions::for_test(is_root, json_output, play_beep)
}

// ---------------------------------------------------------------------------
// Config JSON constants
// ---------------------------------------------------------------------------

pub(crate) fn valid_config_json() -> &'static str {
    r#"{"mount_point":"/mnt/storage"}"#
}

pub(crate) fn config_with_ups_enabled() -> &'static str {
    r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"}}"#
}

pub(crate) fn config_without_ups() -> &'static str {
    r#"{"mount_point":"/mnt/storage"}"#
}

// ---------------------------------------------------------------------------
// `DoctorContext` builders
//
// These wrap the `#[cfg(test)] pub(crate)` constructors in `doctor.rs`. They
// build contexts with `config_path: PathBuf::new()` and no caches populated
// -- intended for tests that call individual `check_*` functions directly.
// Tests that need a non-empty `config_path` (e.g. the dotted-path permissions
// test) reassign `ctx.config_path` after construction.
// ---------------------------------------------------------------------------

pub(crate) fn parsed_doctor_ctx<'a, R: CommandRunner>(
    runner: &'a R,
    paths: &'a StatePaths,
) -> DoctorContext<'a, R> {
    DoctorContext::for_test_parsed(runner, paths, valid_config_json())
}

pub(crate) fn beep_ctx<'a, R: CommandRunner>(
    runner: &'a R,
    paths: &'a StatePaths,
) -> DoctorContext<'a, R> {
    DoctorContext::for_test_beep(runner, paths)
}

pub(crate) fn ups_ctx<'a, R: CommandRunner>(
    runner: &'a R,
    paths: &'a StatePaths,
    config_json: &str,
) -> DoctorContext<'a, R> {
    DoctorContext::for_test_ups(runner, paths, config_json)
}

// ---------------------------------------------------------------------------
// Mock command-output factories
//
// Each pair-returning helper returns `(CmdRequest, RawCommandOutput)` so
// callers can chain `.with_output(req, out)` on a `MockRunner`.
// ---------------------------------------------------------------------------

pub(crate) fn mountpoint_ok() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::MountpointCheck {
            path: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "mountpoint -q /mnt/storage".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

pub(crate) fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::MountpointCheck {
            path: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "mountpoint -q /mnt/storage".into(),
            stdout: String::new(),
            stderr: "/mnt/storage is not a mountpoint\n".into(),
            exit_status: 1,
        },
    )
}

pub(crate) fn df_json(json: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsFilesystemDfJson {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "btrfs --format json filesystem df /mnt/storage".into(),
            stdout: json.into(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

pub(crate) fn df_json_fail() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsFilesystemDfJson {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "btrfs --format json filesystem df /mnt/storage".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        },
    )
}

pub(crate) fn device_usage_healthy() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "btrfs device usage --raw /mnt/storage".into(),
            stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

pub(crate) fn device_usage_with_missing() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "btrfs device usage --raw /mnt/storage".into(),
            stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:                  0

"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

/// Bare `RawCommandOutput` (not paired with a request) because braid-online
/// tests build the `SystemctlIsActive` request inline -- the unit name
/// varies in some skip tests.
pub(crate) fn systemctl_is_active_output(state: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: "systemctl is-active braid-online.service".into(),
        stdout: if state.is_empty() {
            String::new()
        } else {
            format!("{state}\n")
        },
        stderr: String::new(),
        exit_status: match state {
            "active" | "reloading" | "refreshing" => 0,
            _ => 3,
        },
    }
}

// ---------------------------------------------------------------------------
// DF JSON corpora
// ---------------------------------------------------------------------------

pub(crate) const DF_RAID1_CLEAN: &str = r#"{
    "filesystem-df": [
        { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
        { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
        { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
        { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
    ]
}"#;

pub(crate) const DF_MIXED: &str = r#"{
    "filesystem-df": [
        { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
        { "bg-type": "Data", "bg-profile": "single", "total": 8388608, "used": 4194304 },
        { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
        { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
        { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
    ]
}"#;

pub(crate) const DF_MIXED_METADATA: &str = r#"{
    "filesystem-df": [
        { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
        { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
        { "bg-type": "Metadata", "bg-profile": "single", "total": 8388608, "used": 65536 },
        { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
        { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
    ]
}"#;

// ---------------------------------------------------------------------------
// Custom runners
//
// Each struct encodes a sharp negative invariant. Naming them keeps the
// panic / spawn-failure intent legible at the use site.
// ---------------------------------------------------------------------------

/// Mountpoint Ok, `BtrfsFilesystemDfJson` returns `Err(CmdError::Failed)`.
/// Drives the "both profile checks warn off the same cached error"
/// assertion in `profile_checks_warn_when_df_query_errors`.
pub(crate) struct DfQueryFailureRunner;

impl CommandRunner for DfQueryFailureRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::MountpointCheck { path } if path.0 == "/mnt/storage" => {
                Ok(mountpoint_ok().1)
            }
            CmdRequest::BtrfsFilesystemDfJson { mount_point }
                if mount_point.0 == "/mnt/storage" =>
            {
                Err(CmdError::Failed("df query failed".into()))
            }
            _ => Err(CmdError::MissingMock),
        }
    }

    fn run_with_stdin(
        &self,
        _request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        Err(CmdError::MissingMock)
    }
}

/// Mountpoint Ok, device usage healthy, `BtrfsFilesystemDfJson` panics.
/// Pins the invariant that `check_pool_missing_devices` is decoupled from
/// df. Records every call so the test can assert the expected probe set.
#[derive(Default)]
pub(crate) struct PoolMissingDevicesRunner {
    pub(crate) calls: Mutex<Vec<CmdRequest>>,
}

impl CommandRunner for PoolMissingDevicesRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        self.calls.lock().unwrap().push(request.clone());

        match request {
            CmdRequest::MountpointCheck { path } if path.0 == "/mnt/storage" => {
                Ok(mountpoint_ok().1)
            }
            CmdRequest::BtrfsDeviceUsageRaw { mount_point } if mount_point.0 == "/mnt/storage" => {
                Ok(device_usage_healthy().1)
            }
            CmdRequest::BtrfsFilesystemDfJson { .. } => {
                panic!("pool_missing_devices must not query filesystem df")
            }
            _ => Err(CmdError::MissingMock),
        }
    }

    fn run_with_stdin(
        &self,
        _request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        Err(CmdError::MissingMock)
    }
}

/// `UpscQuery` returns `Err(CmdError::Failed)`; everything else returns
/// `Err(CmdError::MissingMock)`. Distinguishes spawn-failure messaging
/// from query-failure messaging in `check_ups_daemon_up`.
pub(crate) struct UpscSpawnFailureRunner;

impl CommandRunner for UpscSpawnFailureRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::UpscQuery { name } => Err(CmdError::Failed(format!(
                "upsc {name}: No such file or directory"
            ))),
            _ => Err(CmdError::MissingMock),
        }
    }

    fn run_with_stdin(
        &self,
        _request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        Err(CmdError::MissingMock)
    }
}

// ---------------------------------------------------------------------------
// Declared-disks summarizer helper
// ---------------------------------------------------------------------------

/// Tuple builder used by the pure summarizer tests. Wrapping
/// `summarize_declared_disks`'s expected input shape keeps each call site
/// to a single line.
pub(crate) fn cls(name: &str, by_id: &str, state: DiskState) -> (String, String, DiskState) {
    (name.to_owned(), by_id.to_owned(), state)
}
