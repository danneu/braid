//! Idle-scope fixtures for `cli/src/idle.rs`'s `mod tests`.
//!
//! Idle tests rely on strict mountinfo/sysfs filesystem behavior and
//! missing-mock subprocess coverage, so this module keeps the fixtures flat
//! and narrow instead of installing a broad idle runner.

use super::shared::mock_ok;
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::filesystem::Filesystem;
use crate::idle::{BusyReason, IdleResult};
use crate::types::MountPoint;
use std::collections::HashMap;
use std::io::ErrorKind;

/// Canonical fsid for idle sysfs fixtures that model the pool filesystem.
pub(crate) const IDLE_FSID: &str = "12345678-1234-1234-1234-123456789abc";

/// Second fsid for idle tests that pin multi-filesystem scan order and coverage.
pub(crate) const IDLE_FSID_OTHER: &str = "deadbeef-dead-beef-dead-beefdeadbeef";

const MOUNTINFO_WITH_BTRFS_TARGET: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n";
const MOUNTINFO_NON_BTRFS_TARGET: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";
const MOUNTINFO_WITHOUT_TARGET: &str = "26 25 0:23 / / rw,noatime shared:1 - ext4 /dev/sda1 rw\n";

/// Strict idle filesystem mock whose unseeded reads and directory listings fail.
///
/// That strictness is load-bearing for tests that prove `cmd_idle` reaches
/// each intended probe boundary and does not skip real fsid paths.
pub(crate) struct IdleMockFs {
    reads: HashMap<String, Result<String, ErrorKind>>,
    list_dirs: HashMap<String, Result<Vec<String>, ErrorKind>>,
}

impl IdleMockFs {
    /// Empty filesystem surface for tests where a missing mountinfo/sysfs seed
    /// is the behavior being asserted.
    pub(crate) fn empty() -> Self {
        Self {
            reads: HashMap::new(),
            list_dirs: HashMap::new(),
        }
    }

    /// Mounted-btrfs surface with no sysfs listing, used for tests that should
    /// stop before the exclusive-operation scan.
    pub(crate) fn mounted_btrfs_only() -> Self {
        Self::empty().seed_mountinfo(MOUNTINFO_WITH_BTRFS_TARGET)
    }

    /// Mountinfo fixture that lacks the configured target, so idle reports the
    /// pool offline before scrub or sysfs probes can run.
    pub(crate) fn offline_mountinfo() -> Self {
        Self::empty().seed_mountinfo(MOUNTINFO_WITHOUT_TARGET)
    }

    /// Mountinfo fixture with a non-btrfs filesystem at the configured target,
    /// pinning that idle treats it as PoolOffline rather than diverging into the
    /// probe_* NotBtrfs error.
    pub(crate) fn non_btrfs_target() -> Self {
        Self::empty().seed_mountinfo(MOUNTINFO_NON_BTRFS_TARGET)
    }

    /// Custom mountinfo fixture for parser-failure tests that keep the bad
    /// input inline at the call site.
    pub(crate) fn with_mountinfo(content: &str) -> Self {
        Self::empty().seed_mountinfo(content)
    }

    /// Typical mounted-btrfs idle surface with one fsid and one explicit
    /// exclusive-operation body.
    pub(crate) fn with_exclop(body: &str) -> Self {
        Self::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID])
            .seed_exclop(IDLE_FSID, body)
    }

    /// Mounted-btrfs surface whose real fsid cannot be read, preserving
    /// fail-closed sysfs coverage while making the error kind visible.
    pub(crate) fn with_exclop_read_error(kind: ErrorKind) -> Self {
        Self::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID])
            .seed_exclop_error(IDLE_FSID, kind)
    }

    /// Seed `/proc/self/mountinfo` while keeping every other read strict.
    pub(crate) fn seed_mountinfo(mut self, content: &str) -> Self {
        self.reads
            .insert("/proc/self/mountinfo".into(), Ok(content.to_string()));
        self
    }

    /// Seed the exact `/sys/fs/btrfs` listing order observed by the test.
    pub(crate) fn seed_btrfs_listing(mut self, entries: &[&str]) -> Self {
        self.list_dirs.insert(
            "/sys/fs/btrfs".into(),
            Ok(entries.iter().map(|s| (*s).to_string()).collect()),
        );
        self
    }

    /// Force the sysfs directory scan to fail with a specific IO error kind.
    pub(crate) fn seed_btrfs_listing_error(mut self, kind: ErrorKind) -> Self {
        self.list_dirs.insert("/sys/fs/btrfs".into(), Err(kind));
        self
    }

    /// Seed one fsid's `exclusive_operation` body without broadening other fsids.
    pub(crate) fn seed_exclop(mut self, fsid: &str, body: &str) -> Self {
        self.reads.insert(
            format!("/sys/fs/btrfs/{fsid}/exclusive_operation"),
            Ok(format!("{body}\n")),
        );
        self
    }

    /// Force one listed fsid's `exclusive_operation` read to fail.
    pub(crate) fn seed_exclop_error(mut self, fsid: &str, kind: ErrorKind) -> Self {
        self.reads.insert(
            format!("/sys/fs/btrfs/{fsid}/exclusive_operation"),
            Err(kind),
        );
        self
    }
}

impl Filesystem for IdleMockFs {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        match self.reads.get(path) {
            Some(Ok(s)) => Ok(s.clone()),
            Some(Err(kind)) => Err(std::io::Error::new(
                *kind,
                format!("IdleMockFs: seeded read error for {path}"),
            )),
            None => Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("IdleMockFs: unexpected path {path}"),
            )),
        }
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        match self.list_dirs.get(path) {
            Some(Ok(v)) => Ok(v.clone()),
            Some(Err(kind)) => Err(std::io::Error::new(
                *kind,
                format!("IdleMockFs: seeded list_dir error for {path}"),
            )),
            None => Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("IdleMockFs: unexpected list_dir {path}"),
            )),
        }
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        unreachable!("IdleMockFs: read-only fixture; create_dir_all must never be called")
    }
}

/// Canonical idle-test mount point used by scrub requests and `cmd_idle` calls.
pub(crate) fn idle_mp() -> MountPoint {
    MountPoint::new("/mnt/storage".into())
}

/// Completed scrub output that lets idle proceed to the sysfs exclop scan.
pub(crate) fn idle_scrub_finished() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        },
        mock_ok(
            "btrfs scrub status --raw /mnt/storage",
            "UUID:             12345678-1234-1234-1234-123456789abc\n\
             Scrub started:    Mon Jan  1 00:00:00 2024\n\
             Status:           finished\n\
             Duration:         0:00:01\n\
             Total to scrub:   1073741824\n\
             Rate:             1073741824/s\n\
             Error summary:    no errors found\n",
        ),
    )
}

/// Concrete 45% running scrub output with btrfs-derived estimates.
pub(crate) fn idle_scrub_running() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        },
        mock_ok(
            "btrfs scrub status --raw /mnt/storage",
            "UUID:             12345678-1234-1234-1234-123456789abc\n\
             Scrub started:    Mon Jan  1 00:00:00 2024\n\
             Status:           running\n\
             Duration:         0:00:05\n\
             Time left:        0:00:06\n\
             ETA:              Mon Jan  1 00:00:11 2024\n\
             Total to scrub:   30408704000\n\
             Bytes scrubbed:   13683916800  (45.00%)\n\
             Rate:             2736783360/s\n\
             Error summary:    no errors found\n",
        ),
    )
}

/// Sparse running-scrub record (parser parity: `scrub_running_minimal`)
/// whose byte counters are absent, so `cmd_idle` must still report busy
/// with `pct: None`. This is a parser-contract / format-drift case, not
/// live btrfs output -- a real running scrub always carries byte counters
/// (`idle_scrub_running` is the percentage-bearing case).
pub(crate) fn idle_scrub_running_no_bytes() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        },
        mock_ok(
            "btrfs scrub status --raw /mnt/storage",
            "UUID:             12345678-1234-1234-1234-123456789abc\n\
             Status:           running\n\
             Error summary:    no errors found\n",
        ),
    )
}

/// Narrow idle runner that seeds only `BtrfsScrubStatus` and leaves removed
/// subprocess probes observable as missing mocks.
pub(crate) fn idle_runner_with_scrub_finished() -> MockRunner {
    let (req, out) = idle_scrub_finished();
    MockRunner::default().with_output(req, out)
}

/// Compose the minimal runner/filesystem pair that reaches the sysfs scan.
pub(crate) fn idle_ready_for_sysfs_check(exclop: &str) -> (MockRunner, IdleMockFs) {
    (
        idle_runner_with_scrub_finished(),
        IdleMockFs::with_exclop(exclop),
    )
}

/// Idle-specific assertion for fail-closed branches where the probe source is
/// part of the user-facing diagnostic contract.
pub(crate) fn assert_idle_busy_unknown_prefix(result: IdleResult, prefix: &str) {
    match result {
        IdleResult::Busy(BusyReason::Unknown(msg)) => {
            assert!(msg.starts_with(prefix), "expected {prefix:?}, got {msg:?}");
        }
        other => panic!("got {other:?}"),
    }
}
