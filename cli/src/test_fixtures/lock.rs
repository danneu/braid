//! Lock-scope fixtures for `cli/src/lock.rs`'s `mod tests`.
//!
//! Lock is a mutating command whose tests rely on exact `MissingMock`
//! contracts, close/forget request ordering, dry-run step output, and
//! umount-vs-mapper error priority. The fixture stays flat so individual
//! tests still compose the precise request set they intend to prove.

use super::shared;
use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput, Step};
use crate::membership::{DiskMember, PoolMembership};
use crate::types::{ByIdPath, DiskName, LuksUuid, MountPoint};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Mount re-exports under lock-prefixed aliases
// ---------------------------------------------------------------------------

pub(crate) use super::mount::{
    NoopSleeper as LockNoopSleeper, err_raw as lock_err_raw, ok_raw as lock_ok_raw,
    test_config as lock_test_config,
};

// ---------------------------------------------------------------------------
// RecordingRunner
// ---------------------------------------------------------------------------

/// Runner that records close and forget requests while preserving
/// `MockRunner`'s strict missing-mock behavior for every other command.
pub(crate) struct RecordingRunner {
    inner: MockRunner,
    close_calls: Mutex<Vec<String>>,
    close_sequences: Mutex<HashMap<String, VecDeque<RawCommandOutput>>>,
    forget_calls: Mutex<Vec<Vec<String>>>,
}

impl RecordingRunner {
    /// Build a recorder around an already-composed runner so each test keeps
    /// ownership of its exact command surface.
    pub(crate) fn new(inner: MockRunner) -> Self {
        Self {
            inner,
            close_calls: Mutex::new(Vec::new()),
            close_sequences: Mutex::new(HashMap::new()),
            forget_calls: Mutex::new(Vec::new()),
        }
    }

    /// Seed a per-mapper close response queue to model retry sequences
    /// without weakening `MockRunner` for unrelated requests.
    pub(crate) fn with_close_sequence(self, mapper: &str, outputs: Vec<RawCommandOutput>) -> Self {
        self.close_sequences
            .lock()
            .unwrap()
            .insert(mapper.to_owned(), outputs.into());
        self
    }

    /// Return close requests in observed order so retry and continue-after-
    /// error tests can assert the executor contract.
    pub(crate) fn close_calls(&self) -> Vec<String> {
        self.close_calls.lock().unwrap().clone()
    }

    /// Return scoped forget requests in observed order so tests can prove
    /// braid never issues the kernel-global no-arg form.
    pub(crate) fn forget_calls(&self) -> Vec<Vec<String>> {
        self.forget_calls.lock().unwrap().clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        if let CmdRequest::CryptsetupClose { mapper } = request {
            self.close_calls.lock().unwrap().push(mapper.clone());
            let mut seqs = self.close_sequences.lock().unwrap();
            if let Some(queue) = seqs.get_mut(mapper)
                && let Some(out) = queue.pop_front()
            {
                return Ok(out);
            }
        }
        if let CmdRequest::BtrfsDeviceScanForget { devices } = request {
            self.forget_calls.lock().unwrap().push(devices.clone());
        }
        self.inner.run(request)
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        self.inner.run_with_stdin(request, stdin)
    }
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Build the lock-canonical filesystem mock while deriving `/dev/mapper`
/// entries from seeded paths, matching the local mock it replaces.
pub(crate) fn lock_fs(paths: &[&str]) -> shared::MockFs {
    let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
    let mapper_entries: Vec<String> = paths
        .iter()
        .filter_map(|path| path.strip_prefix("/dev/mapper/").map(str::to_owned))
        .filter(|entry| !entry.contains('/'))
        .collect();
    let mapper_entry_refs: Vec<&str> = mapper_entries.iter().map(String::as_str).collect();

    shared::MockFs::storage(paths).with_dev_mapper(&mapper_entry_refs)
}

// ---------------------------------------------------------------------------
// Pool fixture
// ---------------------------------------------------------------------------

/// Canonical lock-test membership keyed by the lock-test seed range
/// (700-799). Two members named `aaa` and `bbb` so the existing close
/// and forget tests address the same mapper names they did before the
/// LUKS-UUID-as-identity migration.
pub(crate) fn lock_test_membership() -> PoolMembership {
    let mut m = PoolMembership::empty();
    m.insert(
        LuksUuid::parse("00000000-0000-0000-0000-0000000002bc").unwrap(),
        DiskMember::new(
            DiskName::parse("aaa").unwrap(),
            ByIdPath::parse("/dev/disk/by-id/a").unwrap(),
        ),
    )
    .unwrap();
    m.insert(
        LuksUuid::parse("00000000-0000-0000-0000-0000000002bd").unwrap(),
        DiskMember::new(
            DiskName::parse("bbb").unwrap(),
            ByIdPath::parse("/dev/disk/by-id/b").unwrap(),
        ),
    )
    .unwrap();
    m
}

// ---------------------------------------------------------------------------
// Composite runners
// ---------------------------------------------------------------------------

/// Add the mounted-pool probe results. `probe_pool` calls
/// `BtrfsFilesystemShow` and then a per-device cryptsetup status +
/// luksUUID pair for each device. The recording-runner fixtures
/// historically only seeded the FsidOnly probe surface; for the Full
/// arm to drive UUID-based classification the per-device probes are
/// also seeded by default. Tests that intentionally trigger the
/// FsidOnly fallback override one of these to fail; tests that want
/// a stranded-mapper scenario add extra per-mapper probes.
pub(crate) fn lock_with_fsid_probe_mocks(runner: MockRunner) -> MockRunner {
    let runner = runner.with_output(
        CmdRequest::BtrfsFilesystemShow {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        RawCommandOutput {
            cmd: "btrfs filesystem show /mnt/storage".into(),
            stdout: "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                         \tTotal devices 2 FS bytes used 16.00MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-aaa\n\
                         \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-bbb\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        },
    );
    // Per-device probe responses for the Full-arm classifier. The
    // member UUIDs are the same ones lock_test_membership inserts so
    // probe_pool -> by_uuid yields MemberOwned.
    runner
        .with_mapper_open(
            "braid-aaa",
            "/dev/disk/by-id/a",
            "00000000-0000-0000-0000-0000000002bc",
        )
        .with_mapper_open(
            "braid-bbb",
            "/dev/disk/by-id/b",
            "00000000-0000-0000-0000-0000000002bd",
        )
}

/// Pre-built mounted lock runner with umount and scoped forget success,
/// leaving mapper-close requests for each test to compose explicitly.
pub(crate) fn lock_mounted_runner() -> MockRunner {
    lock_with_fsid_probe_mocks(MockRunner::default().with_output(
        CmdRequest::MountpointCheck {
            path: MountPoint("/mnt/storage".to_owned()),
        },
        lock_ok_raw("mountpoint -q /mnt/storage"),
    ))
    .with_output(
        CmdRequest::Umount {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        lock_ok_raw("umount /mnt/storage"),
    )
    .with_output(
        CmdRequest::BtrfsDeviceScanForget {
            devices: vec![
                "/dev/mapper/braid-aaa".into(),
                "/dev/mapper/braid-bbb".into(),
            ],
        },
        lock_ok_raw("btrfs device scan --forget"),
    )
}

/// Pre-built mounted runner whose umount fails busy and whose forget request
/// is intentionally absent because forget is gated on successful unmount.
pub(crate) fn lock_umount_failed_runner() -> MockRunner {
    lock_with_fsid_probe_mocks(MockRunner::default().with_output(
        CmdRequest::MountpointCheck {
            path: MountPoint("/mnt/storage".to_owned()),
        },
        lock_ok_raw("mountpoint -q /mnt/storage"),
    ))
    .with_output(
        CmdRequest::Umount {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        },
        lock_err_raw("umount /mnt/storage", 32, "target is busy"),
    )
}

// ---------------------------------------------------------------------------
// Dry-run assertion helpers
// ---------------------------------------------------------------------------

/// Extract the single scoped forget device list from compiled lock steps so
/// tests assert the command payload, not just rendered text.
pub(crate) fn lock_forget_step_devices(steps: &[Step]) -> Vec<String> {
    let mut found: Option<Vec<String>> = None;
    for step in steps {
        for cmd in &step.commands {
            if let CmdRequest::BtrfsDeviceScanForget { devices } = cmd {
                assert!(found.is_none(), "multiple forget steps in plan: {steps:?}");
                found = Some(devices.clone());
            }
        }
    }
    found.expect("no forget step in plan")
}

/// Count forget steps so tests can prove the empty-mapper branch omits the
/// command entirely rather than issuing a kernel-global no-arg forget.
pub(crate) fn lock_count_forget_steps(steps: &[Step]) -> usize {
    steps
        .iter()
        .flat_map(|s| &s.commands)
        .filter(|c| matches!(c, CmdRequest::BtrfsDeviceScanForget { .. }))
        .count()
}
