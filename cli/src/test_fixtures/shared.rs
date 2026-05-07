//! Cross-scope fixture core: `mock_ok`, `MockFs`, and the `PoolFixture`
//! struct + ctors shared by every command scope.

use crate::cmd::RawCommandOutput;
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::path::PathBuf;
use tempfile::TempDir;

/// Compact constructor for a successful `RawCommandOutput`. Mirrors the
/// per-file `mock_ok` helper that lived in `replace.rs`/`add.rs` so
/// migrated tests stay one line.
pub(crate) fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: cmd.to_owned(),
        stdout: stdout.to_owned(),
        stderr: String::new(),
        exit_status: 0,
    }
}

// ---------------------------------------------------------------------------
// MockFs
// ---------------------------------------------------------------------------

/// Generic `Filesystem` mock: a configurable set of paths reported as
/// existing, the canonical `/proc/self/mountinfo` body, and an
/// overridable sysfs `exclusive_operation` body for preflight tests.
pub(crate) struct MockFs {
    paths: Vec<String>,
    mountinfo: String,
    excl_op: String,
}

impl MockFs {
    /// Mounted /mnt/storage (the default fixture mountpoint), no
    /// in-flight exclusive operation.
    pub(crate) fn storage(paths: Vec<String>) -> Self {
        Self {
            paths,
            mountinfo: "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                .into(),
            excl_op: "none\n".into(),
        }
    }

    /// Unmounted host: `/proc/self/mountinfo` reports the rootfs only,
    /// no /mnt/storage entry. Use for bootstrap tests where the pool
    /// is not yet mounted at the start of the run.
    #[allow(dead_code)]
    pub(crate) fn unmounted(paths: Vec<String>) -> Self {
        Self {
            paths,
            mountinfo: "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".into(),
            excl_op: "none\n".into(),
        }
    }

    /// Override the sysfs exclusive_operation body. Use to drive
    /// preflight's busy-op / paused-balance branches.
    pub(crate) fn with_excl_op(mut self, body: &str) -> Self {
        self.excl_op = body.to_owned();
        self
    }
}

impl Filesystem for MockFs {
    fn exists(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p == path)
    }
    fn is_block_device(&self, _path: &str) -> bool {
        false
    }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            Ok(self.mountinfo.clone())
        } else if path.ends_with("/exclusive_operation") {
            Ok(self.excl_op.clone())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// PoolFixture
// ---------------------------------------------------------------------------

/// Bundled tempdirs + paths + config + passphrase + inhibitor for any
/// command that takes `ReplaceParams` (and, in follow-ups, `AddParams`,
/// `RemoveParams`). `_state_tmp` and `_config_tmp` are RAII guards that
/// keep the temp directories alive for as long as the fixture lives.
pub(crate) struct PoolFixture {
    pub(in crate::test_fixtures) _state_tmp: TempDir,
    pub(crate) paths: StatePaths,
    pub(in crate::test_fixtures) _config_tmp: TempDir,
    pub(crate) config_path: PathBuf,
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
}

impl PoolFixture {
    /// Build the temp directories + canonical config.json + passphrase
    /// file used by every constructor.
    pub(in crate::test_fixtures) fn empty_inner() -> (TempDir, StatePaths, TempDir, PathBuf, PathBuf)
    {
        let state_tmp = tempfile::tempdir().expect("state tempdir");
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().expect("config tempdir");
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .expect("write config.json");
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").expect("write passphrase file");
        (state_tmp, paths, config_tmp, config_path, pass_path)
    }

    /// pool.json: disk1 + disk2 (live, no devid pinned). Use for live
    /// replace tests where btrfs reports both members live.
    pub(crate) fn two_disk_healthy() -> Self {
        let (state_tmp, paths, config_tmp, config_path, pass_path) = Self::empty_inner();
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).expect("save_membership");
        Self {
            _state_tmp: state_tmp,
            paths,
            _config_tmp: config_tmp,
            config_path,
            pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }

    /// pool.json: disk1 (no devid) + disk2 (devid=2). Models the missing
    /// path with explicit devid pinning so `build_replacement_membership`
    /// can match `--missing-id 2` to the disk2 row.
    pub(crate) fn one_live_one_missing() -> Self {
        let (state_tmp, paths, config_tmp, config_path, pass_path) = Self::empty_inner();
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).expect("save_membership");
        Self {
            _state_tmp: state_tmp,
            paths,
            _config_tmp: config_tmp,
            config_path,
            pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }

    /// No pool.json seeded. Use for validation-only tests that abort
    /// before any membership probe (e.g. PanicRunner-backed boundary
    /// tests).
    pub(crate) fn empty() -> Self {
        let (state_tmp, paths, config_tmp, config_path, pass_path) = Self::empty_inner();
        Self {
            _state_tmp: state_tmp,
            paths,
            _config_tmp: config_tmp,
            config_path,
            pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }
}
