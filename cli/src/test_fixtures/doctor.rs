//! Doctor scope fixtures: cross-test scaffolding for `cli/src/doctor.rs`'s
//! `mod tests`.
//!
//! Doctor is check-oriented (read state, render result), not mutating-command
//! oriented, so this module is a flat collection of helpers rather than the
//! `*Pool` + `*ParamsBuilder` + `PoolFixture` triad the other commands use.
//!
//! Field-construction of `DoctorContext` happens via the `#[cfg(test)]
//! pub(crate)` constructor on that type in `doctor.rs` (see the "Test-only
//! constructors" section there); doctor.rs holds the private fields, so this
//! module cannot field-literal-construct it directly.

use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
use crate::doctor::{DiskState, DoctorContext, DoctorOptions};
use crate::probe::Filesystem;
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

// ---------------------------------------------------------------------------
// Config JSON constants
// ---------------------------------------------------------------------------

pub(crate) fn valid_config_json() -> &'static str {
    r#"{"mount_point":"/mnt/storage"}"#
}

pub(crate) fn config_with_ups_enabled() -> &'static str {
    r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"},"systemd_lifecycle":true}"#
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
    DoctorContext::for_test_parsed(runner, paths, config_json)
}

// ---------------------------------------------------------------------------
// Filesystem fixtures
// ---------------------------------------------------------------------------

/// Strict doctor filesystem mock for live-pool checks that must prove
/// `probe_pool` reads mountinfo through doctor's injected filesystem.
pub(crate) struct DoctorMockFs {
    mountinfo: String,
}

impl DoctorMockFs {
    /// Mounted btrfs pool surface with no broader host filesystem access.
    pub(crate) fn mounted_btrfs_only() -> Self {
        Self {
            mountinfo:
                "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                    .into(),
        }
    }

    /// Empty host mount table for tests that must stop before live-pool
    /// probing reads mountinfo.
    pub(crate) fn empty() -> Self {
        Self {
            mountinfo: String::new(),
        }
    }
}

impl Filesystem for DoctorMockFs {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            Ok(self.mountinfo.clone())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("DoctorMockFs: unexpected path {path}"),
            ))
        }
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }
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
/// tests build the `SystemctlShowActiveState` request inline -- the unit name
/// varies in some skip tests.
pub(crate) fn systemctl_show_active_state_output(state: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: "systemctl show -P ActiveState braid-online.service".into(),
        stdout: if state.is_empty() {
            String::new()
        } else {
            format!("{state}\n")
        },
        stderr: String::new(),
        exit_status: 0,
    }
}

pub(crate) fn smartctl_selftest_json(
    device: &str,
    fixture_name: &str,
    exit_status: i32,
) -> (CmdRequest, RawCommandOutput) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nixos-25.11");
    let stdout =
        std::fs::read_to_string(format!("{dir}/{fixture_name}")).expect("selftest fixture reads");
    (
        CmdRequest::SmartctlSelftestLogJson {
            device: device.to_owned(),
        },
        RawCommandOutput {
            cmd: format!("smartctl --json -A -l selftest {device}"),
            stdout,
            stderr: String::new(),
            exit_status,
        },
    )
}

pub(crate) fn smart_selftest_runner_for(devices: &[(&str, &str, i32)]) -> MockRunner {
    devices.iter().fold(
        MockRunner::default(),
        |runner, (device, fixture, exit_status)| {
            let (request, output) = smartctl_selftest_json(device, fixture, *exit_status);
            runner.with_output(request, output)
        },
    )
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
