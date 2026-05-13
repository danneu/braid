//! Cross-scope fixture core: `mock_ok`, `MockFs`, and the `PoolFixture`
//! struct + ctors shared by every command scope.

use crate::cmd::RawCommandOutput;
use crate::config::Config;
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, DiskName, LuksUuid, MountPoint};
use std::path::PathBuf;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// UUID-keyed fixture helpers (introduced ahead of the recover.rs / per-command
// fixture rekey so subsequent fixture rekeys are mechanical).
// ---------------------------------------------------------------------------

/// Deterministic fixture-only `LuksUuid` generator. The first 20 hex
/// digits are zero so `seed` is the only varying bits, which makes
/// UUID-lex order identical to seed order. Production code never calls
/// this helper -- it is the single source of truth for test UUIDs so
/// per-module seed allocation (see plan) yields stable on-disk diffs.
pub(crate) fn test_uuid(seed: u64) -> LuksUuid {
    LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
        .expect("hand-padded UUID is canonical")
}

/// Build a `(LuksUuid, DiskMember)` pair for use as the value half of a
/// `PoolMembership::insert(uuid, member)` call. Devid and added_at
/// default to `None`; tests that need them populated use
/// `disk_member_with`.
pub(crate) fn disk_member(seed: u64, name: &str, by_id: &str) -> (LuksUuid, DiskMember) {
    (
        test_uuid(seed),
        DiskMember {
            name: DiskName::parse(name).expect("valid disk name in fixture"),
            by_id: ByIdPath::parse(by_id).expect("valid by-id path in fixture"),
            devid: None,
            added_at: None,
        },
    )
}

/// Build a `(LuksUuid, DiskMember)` pair with an explicit `devid` and/or
/// `added_at`. Same shape as `disk_member` so fixture call sites stay
/// one-line for the common case.
#[allow(dead_code)]
pub(crate) fn disk_member_with(
    seed: u64,
    name: &str,
    by_id: &str,
    devid: Option<u64>,
    added_at: Option<&str>,
) -> (LuksUuid, DiskMember) {
    let (uuid, mut m) = disk_member(seed, name, by_id);
    m.devid = devid;
    m.added_at = added_at.map(|s| s.to_owned());
    (uuid, m)
}

/// Single source of truth for the passphrase bytes the fixture writes to
/// `pass_path` and that scope-local stdin expectations match against.
/// `read_passphrase` strips the trailing newline from the file body, so
/// these are the bytes that reach `cryptsetup` over stdin.
pub(crate) const TEST_PASSPHRASE_BYTES: &[u8] = b"test-passphrase";

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

/// Generic `Filesystem` mock: configurable existing paths, mountinfo,
/// sysfs `exclusive_operation`, and `/dev/mapper` listing behavior for
/// command tests that need filesystem probes without touching the host.
pub(crate) struct MockFs {
    paths: Vec<String>,
    mountinfo: String,
    excl_op: String,
    dev_mapper: DevMapperListing,
}

/// `/dev/mapper` listing behavior, kept explicit so tests can distinguish
/// an empty directory, a populated directory, and an unreadable directory.
enum DevMapperListing {
    Empty,
    Entries(Vec<String>),
    Error(std::io::ErrorKind),
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
            dev_mapper: DevMapperListing::Empty,
        }
    }

    /// Unmounted host: `/proc/self/mountinfo` reports the rootfs only,
    /// no /mnt/storage entry. Use for bootstrap tests where the pool
    /// is not yet mounted at the start of the run.
    pub(crate) fn unmounted(paths: Vec<String>) -> Self {
        Self {
            paths,
            mountinfo: "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".into(),
            excl_op: "none\n".into(),
            dev_mapper: DevMapperListing::Empty,
        }
    }

    /// Override `/proc/self/mountinfo` for tests that exercise mounted
    /// non-btrfs, malformed, or otherwise custom mount table branches.
    pub(crate) fn with_mountinfo(mut self, body: &str) -> Self {
        self.mountinfo = body.to_owned();
        self
    }

    /// Override the sysfs exclusive_operation body. Use to drive
    /// preflight's busy-op / paused-balance branches.
    pub(crate) fn with_excl_op(mut self, body: &str) -> Self {
        self.excl_op = body.to_owned();
        self
    }

    /// Override `list_dir("/dev/mapper")` for tests that need a mapper
    /// listing distinct from the set of paths reported by `exists`.
    pub(crate) fn with_dev_mapper(mut self, entries: &[&str]) -> Self {
        self.dev_mapper =
            DevMapperListing::Entries(entries.iter().map(|entry| (*entry).to_owned()).collect());
        self
    }

    /// Make `list_dir("/dev/mapper")` fail with PermissionDenied so
    /// callers can verify degraded orphan-scan behavior.
    pub(crate) fn with_dev_mapper_error(mut self) -> Self {
        self.dev_mapper = DevMapperListing::Error(std::io::ErrorKind::PermissionDenied);
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
    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        if path == "/dev/mapper" || path == "/dev/mapper/" {
            match &self.dev_mapper {
                DevMapperListing::Empty => Ok(vec![]),
                DevMapperListing::Entries(entries) => Ok(entries.clone()),
                DevMapperListing::Error(kind) => {
                    Err(std::io::Error::new(*kind, "permission denied"))
                }
            }
        } else {
            Ok(vec![])
        }
    }
}

// ---------------------------------------------------------------------------
// PoolFixture
// ---------------------------------------------------------------------------

/// Bundled tempdirs + paths + config + passphrase + inhibitor for any
/// command that takes `ReplaceParams` (and, in follow-ups, `AddParams`,
/// `RemoveParams`, `RecoverParams`). `_state_tmp` and `_config_tmp` are
/// RAII guards that keep the temp directories alive for as long as the
/// fixture lives. `config` is owned here so the per-scope params builders
/// can borrow `&'a Config` without round-tripping through disk.
pub(crate) struct PoolFixture {
    pub(in crate::test_fixtures) _state_tmp: TempDir,
    pub(crate) paths: StatePaths,
    pub(in crate::test_fixtures) _config_tmp: TempDir,
    pub(crate) config_path: PathBuf,
    pub(crate) config: Config,
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
}

/// Common ground produced by `empty_inner`. Bundled into a struct so
/// adding a new piece (e.g. the canonical `Config`) does not force every
/// caller to update its destructure.
pub(in crate::test_fixtures) struct PoolFixtureBase {
    pub(in crate::test_fixtures) state_tmp: TempDir,
    pub(in crate::test_fixtures) paths: StatePaths,
    pub(in crate::test_fixtures) config_tmp: TempDir,
    pub(in crate::test_fixtures) config_path: PathBuf,
    pub(in crate::test_fixtures) config: Config,
    pub(in crate::test_fixtures) pass_path: PathBuf,
}

impl PoolFixture {
    /// Build the temp directories + canonical config.json + passphrase
    /// file used by every constructor. The same `MountPoint` drives both
    /// the on-disk config.json (so `config_path` round-trips through
    /// `Config::load`) and the in-memory `Config` returned alongside.
    pub(in crate::test_fixtures) fn empty_inner() -> PoolFixtureBase {
        let state_tmp = tempfile::tempdir().expect("state tempdir");
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().expect("config tempdir");
        let config_path = config_tmp.path().join("config.json");
        let mount_point = MountPoint("/mnt/storage".into());
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": mount_point.0 })).unwrap(),
        )
        .expect("write config.json");
        let config = Config::new(mount_point).expect("config from canonical mount_point");
        let pass_path = config_tmp.path().join("passphrase");
        let mut pass_bytes = TEST_PASSPHRASE_BYTES.to_vec();
        pass_bytes.push(b'\n');
        std::fs::write(&pass_path, &pass_bytes).expect("write passphrase file");
        PoolFixtureBase {
            state_tmp,
            paths,
            config_tmp,
            config_path,
            config,
            pass_path,
        }
    }

    /// pool.json: disk1 + disk2 (live, no devid pinned). Use for live
    /// replace tests where btrfs reports both members live.
    pub(crate) fn two_disk_healthy() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        let (_, member) = disk_member(1, "disk1", "/dev/disk/by-id/virtio-disk1");
        let uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111")
            .expect("canonical fixture UUID");
        m.insert(uuid, member).expect("insert disk1");
        let (_, member) = disk_member(2, "disk2", "/dev/disk/by-id/virtio-disk2");
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222")
            .expect("canonical fixture UUID");
        m.insert(uuid, member).expect("insert disk2");
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config_path: base.config_path,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }

    /// pool.json: disk1 (no devid) + disk2 (devid=2). Models the missing
    /// path with explicit devid pinning so `build_replacement_membership`
    /// can match `--missing-id 2` to the disk2 row.
    pub(crate) fn one_live_one_missing() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        let (_, member) = disk_member(1, "disk1", "/dev/disk/by-id/virtio-disk1");
        let uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111")
            .expect("canonical fixture UUID");
        m.insert(uuid, member).expect("insert disk1");
        let (_, member) =
            disk_member_with(2, "disk2", "/dev/disk/by-id/virtio-disk2", Some(2), None);
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222")
            .expect("canonical fixture UUID");
        m.insert(uuid, member).expect("insert disk2");
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config_path: base.config_path,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }

    /// No pool.json seeded. Use for validation-only tests that abort
    /// before any membership probe (e.g. PanicRunner-backed boundary
    /// tests) and for recover tests that drive state through the journal
    /// rather than pool.json.
    pub(crate) fn empty() -> Self {
        let base = Self::empty_inner();
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config_path: base.config_path,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }
}
