//! Cross-scope fixture core: `mock_ok`, `MockFs`, and the `PoolFixture`
//! struct + ctors shared by every command scope.

use crate::cmd::{CmdRequest, LsblkFieldKind, MockRunner, RawCommandOutput};
use crate::config::Config;
use crate::confirm::RecordingConfirm;
use crate::inhibit::RecordingInhibitor;
use crate::luks::BackingPathResolver;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::progress::Sleeper;
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, Devid, DiskName, LuksUuid, MountPoint};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

/// Canonical repeated-digit fixture LUKS UUID for disk `n`: `n` is the hex
/// digit repeated across all 32 positions (`canonical_luks_uuid(2)` ->
/// `22222222-2222-2222-2222-222222222222`). This is the UUID the present-pool
/// `RemovalPool` live probe (`luks_uuid_for_device`) reports for `/dev/vd{b,c,d}`.
///
/// Use `canonical_luks_uuid(n)` for any fixture entry modeling the canonical
/// `diskN` identity -- present OR temporarily missing (e.g. the replace target
/// in `one_live_one_missing`). For a PRESENT, live-probed disk the pool.json key
/// MUST equal the probed UUID or the membership<->live-UUID correlation is a
/// silent incidental pass; for a missing `diskN` it keeps the row recognizable
/// and future-proof if that disk later becomes present.
///
/// Reserve `test_uuid(seed)` (`00000000-...-{seed}`) for identities that are NOT
/// a canonical `diskN`: arbitrary unique values and deliberately custom-mocked
/// sentinels (drift / foreign targets whose `CryptsetupLuksUuid` probe is
/// overridden to that exact value).
pub(crate) fn canonical_luks_uuid(n: u64) -> LuksUuid {
    // Fail closed: n == 0 would silently build the nil UUID, and n > 15 (or a
    // value that truncates under `as u32`) would alias another disk -- both
    // defeat the single-source purpose. Assert the full u64 before casting.
    assert!(
        (1..=15).contains(&n),
        "canonical disk index must be 1..=15, got {n}"
    );
    let d = std::char::from_digit(n as u32, 16).expect("1..=15 is a single hex digit");
    let g = |len: usize| -> String { std::iter::repeat_n(d, len).collect() };
    LuksUuid::parse(&format!("{}-{}-{}-{}-{}", g(8), g(4), g(4), g(4), g(12)))
        .expect("canonical repeated-digit UUID is valid")
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
pub(crate) fn disk_member_with(
    seed: u64,
    name: &str,
    by_id: &str,
    devid: Option<Devid>,
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

/// Render a `btrfs device remove` failure stderr line in btrfs-progs' by-id
/// shape. A bare numeric devid takes the `string_is_numerical` ->
/// `BTRFS_DEVICE_SPEC_BY_ID` arm in `reference/btrfs-progs/cmds/device.c`,
/// printing `error removing devid <n>` -- the word "devid", no quotes. braid's
/// `remove-missing` always removes by devid, so its failure fixtures use this.
pub(crate) fn btrfs_remove_devid_error(devid: u64, msg: &str) -> String {
    format!("ERROR: error removing devid {devid}: {msg}")
}

/// Render a `btrfs device remove` failure stderr line in btrfs-progs' by-path
/// shape: a block-device argument prints `error removing device '<path>'`
/// (quoted), per the non-`is_devid` arm in `reference/btrfs-progs/cmds/device.c`.
/// braid's live `remove` removes by mapper path, so its failure fixtures use this.
pub(crate) fn btrfs_remove_path_error(path: &str, msg: &str) -> String {
    format!("ERROR: error removing device '{path}': {msg}")
}

/// Register `lsblk` Model/Serial/Size outputs for `device` so a confirm
/// prompt's hw line resolves only when the probe is routed to THIS path.
/// Lets routing tests pin that a present-disk prompt queries the live
/// backing path (decision 024) and a target prompt queries the by-id handle:
/// `query_disk_hw_info` swallows a `MissingMock` to `None`, so a probe sent
/// to any other path leaves the hw line blank and fails the assertion.
///
/// Emits exit-0 outputs via `mock_ok`; `get_lsblk_field` trims them and
/// parses `Size` with `parse::<u64>()`, so `size` is rendered as its integer.
/// `.with_output` resolves only after a fixture's `with_handler` closures
/// return `None` for `LsblkField`, so wrapping a fixture-installed runner
/// falls through to these cleanly.
pub(crate) fn with_lsblk_hw_info(
    runner: MockRunner,
    device: &str,
    model: &str,
    serial: &str,
    size: u64,
) -> MockRunner {
    runner
        .with_output(
            CmdRequest::LsblkField {
                device: device.to_owned(),
                field: LsblkFieldKind::Model,
            },
            mock_ok("lsblk", model),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: device.to_owned(),
                field: LsblkFieldKind::Serial,
            },
            mock_ok("lsblk", serial),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: device.to_owned(),
                field: LsblkFieldKind::Size,
            },
            mock_ok("lsblk", &format!("{size}")),
        )
}

/// Device stanza spec for faithful `btrfs device usage --raw` fixture output.
/// Mirrors the fields braid's parser consumes so command tests share one
/// btrfs-progs-shaped raw-output source.
pub(crate) struct DeviceUsageSpec {
    /// `None` renders the missing-device header `<missing disk>, ID: N` --
    /// the kernel's `btrfs_dev_name()` marker as copied through
    /// `BTRFS_IOC_DEV_INFO` by `btrfs device usage`.
    pub(crate) path: Option<String>,
    pub(crate) devid: u64,
    pub(crate) device_size: u64,
    pub(crate) device_slack: u64,
    pub(crate) allocations: Vec<(String, String, u64)>,
    pub(crate) unallocated: u64,
}

impl DeviceUsageSpec {
    /// Build a live-device stanza; current fixtures all use zero device slack.
    pub(crate) fn live(
        path: &str,
        devid: u64,
        device_size: u64,
        allocations: &[(&str, &str, u64)],
        unallocated: u64,
    ) -> Self {
        Self {
            path: Some(path.to_owned()),
            devid,
            device_size,
            device_slack: 0,
            allocations: allocations
                .iter()
                .map(|(kind, profile, bytes)| ((*kind).to_owned(), (*profile).to_owned(), *bytes))
                .collect(),
            unallocated,
        }
    }

    /// Build a missing-device stanza using the pinned kernel path marker.
    pub(crate) fn missing(devid: u64, allocations: &[(&str, &str, u64)], unallocated: u64) -> Self {
        Self {
            path: None,
            devid,
            device_size: 0,
            device_slack: 0,
            allocations: allocations
                .iter()
                .map(|(kind, profile, bytes)| ((*kind).to_owned(), (*profile).to_owned(), *bytes))
                .collect(),
            unallocated,
        }
    }
}

/// Render faithful `btrfs device usage --raw` stdout for test fixtures.
/// Uses 3-space key/value indentation, the kernel-sourced `<missing disk>`
/// marker for absent devices, and a blank line after every device stanza.
pub(crate) fn device_usage_raw_body(specs: &[DeviceUsageSpec]) -> String {
    fn push_kv(body: &mut String, label: &str, value: u64) {
        let width = 33usize.saturating_sub(3 + label.len());
        body.push_str(&format!("   {label}:{value:>width$}\n"));
    }

    let mut body = String::new();
    for spec in specs {
        let path = spec.path.as_deref().unwrap_or("<missing disk>");
        body.push_str(&format!("{path}, ID: {}\n", spec.devid));
        push_kv(&mut body, "Device size", spec.device_size);
        push_kv(&mut body, "Device slack", spec.device_slack);
        for (alloc_type, profile, bytes) in &spec.allocations {
            push_kv(&mut body, &format!("{alloc_type},{profile}"), *bytes);
        }
        push_kv(&mut body, "Unallocated", spec.unallocated);
        body.push('\n');
    }
    body
}

/// Shared sleeper test double for command seams that must prove retry
/// or polling code used the injected sleeper instead of wall-clock sleep.
#[derive(Clone, Default)]
pub(crate) struct RecordingSleeper {
    calls: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingSleeper {
    /// Return a snapshot of recorded sleep durations for assertions.
    pub(crate) fn calls(&self) -> Vec<Duration> {
        self.calls.lock().unwrap().clone()
    }
}

impl Sleeper for RecordingSleeper {
    fn sleep(&self, duration: Duration) {
        self.calls.lock().unwrap().push(duration);
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
    /// When `Some`, `create_dir_all` fails with this error kind instead of
    /// succeeding, so mount/pool failure tests can exercise the fail-closed
    /// mount-point-creation path without touching a real filesystem.
    create_dir_error: Option<std::io::ErrorKind>,
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
            create_dir_error: None,
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
            create_dir_error: None,
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

    /// Make `create_dir_all` fail with `kind` so mount/pool tests can drive
    /// the fail-closed mount-point-creation path. Default is success.
    pub(crate) fn with_create_dir_error(mut self, kind: std::io::ErrorKind) -> Self {
        self.create_dir_error = Some(kind);
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
    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        match self.create_dir_error {
            Some(kind) => Err(std::io::Error::new(kind, "mock create_dir_all failure")),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// MockBackingPathResolver
// ---------------------------------------------------------------------------

/// Test resolver for mapper ownership checks. Unknown paths resolve to
/// themselves so tests only seed by-id -> kernel-path pairs they care about.
#[derive(Debug, Clone, Default)]
pub(crate) struct MockBackingPathResolver {
    overrides: BTreeMap<String, Result<String, std::io::ErrorKind>>,
}

impl MockBackingPathResolver {
    /// Seed a successful canonical path override.
    pub(crate) fn with_path(mut self, path: &str, canonical: &str) -> Self {
        self.overrides
            .insert(path.to_owned(), Ok(canonical.to_owned()));
        self
    }

    /// Seed a failing canonical path override.
    pub(crate) fn with_error(mut self, path: &str, kind: std::io::ErrorKind) -> Self {
        self.overrides.insert(path.to_owned(), Err(kind));
        self
    }

    /// Common virtio-disk fixture mapping used by command-level tests.
    pub(crate) fn with_virtio_defaults() -> Self {
        Self::default()
            .with_path("/dev/disk/by-id/virtio-disk1", "/dev/vda")
            .with_path("/dev/disk/by-id/virtio-disk2", "/dev/vdb")
            .with_path("/dev/disk/by-id/virtio-disk3", "/dev/vdc")
            .with_path("/dev/disk/by-id/virtio-disk4", "/dev/vdd")
    }

    /// Virtio fixture mapping for add/replace topologies whose first
    /// modeled data disk starts at `/dev/vdb`.
    pub(crate) fn with_virtio_offset_defaults() -> Self {
        Self::default()
            .with_path("/dev/disk/by-id/virtio-disk1", "/dev/vdb")
            .with_path("/dev/disk/by-id/virtio-disk2", "/dev/vdc")
            .with_path("/dev/disk/by-id/virtio-disk3", "/dev/vdd")
            .with_path("/dev/disk/by-id/virtio-disk4", "/dev/vde")
    }
}

impl BackingPathResolver for MockBackingPathResolver {
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
        match self.overrides.get(path) {
            Some(Ok(canonical)) => Ok(canonical.clone()),
            Some(Err(kind)) => Err(std::io::Error::new(*kind, "mock canonicalize error")),
            None => Ok(path.to_owned()),
        }
    }
}

/// Shared resolver seeded with the standard virtio-disk fixture mapping.
pub(crate) fn mock_virtio_backing_path_resolver() -> &'static MockBackingPathResolver {
    static RESOLVER: std::sync::OnceLock<MockBackingPathResolver> = std::sync::OnceLock::new();
    RESOLVER.get_or_init(MockBackingPathResolver::with_virtio_defaults)
}

/// Shared resolver for add/replace fixtures that model disks as `/dev/vdb+`.
pub(crate) fn mock_virtio_offset_backing_path_resolver() -> &'static MockBackingPathResolver {
    static RESOLVER: std::sync::OnceLock<MockBackingPathResolver> = std::sync::OnceLock::new();
    RESOLVER.get_or_init(MockBackingPathResolver::with_virtio_offset_defaults)
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
    pub(crate) config: Config,
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
    pub(crate) confirm: RecordingConfirm,
}

/// Common ground produced by `empty_inner`. Bundled into a struct so
/// adding a new piece (e.g. the canonical `Config`) does not force every
/// caller to update its destructure.
pub(in crate::test_fixtures) struct PoolFixtureBase {
    pub(in crate::test_fixtures) state_tmp: TempDir,
    pub(in crate::test_fixtures) paths: StatePaths,
    pub(in crate::test_fixtures) config_tmp: TempDir,
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
        let mount_point = MountPoint::new("/mnt/storage".into());
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": mount_point.as_str() }))
                .unwrap(),
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
        m.insert(canonical_luks_uuid(1), member)
            .expect("insert disk1");
        let (_, member) = disk_member(2, "disk2", "/dev/disk/by-id/virtio-disk2");
        m.insert(canonical_luks_uuid(2), member)
            .expect("insert disk2");
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
            confirm: RecordingConfirm::new(),
        }
    }

    /// pool.json: disk1 (no devid) + disk2 (devid=2). Models the missing
    /// path with explicit devid pinning so `build_replacement_membership`
    /// can match `--missing-id 2` to the disk2 row.
    pub(crate) fn one_live_one_missing() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        let (_, member) = disk_member(1, "disk1", "/dev/disk/by-id/virtio-disk1");
        m.insert(canonical_luks_uuid(1), member)
            .expect("insert disk1");
        let (_, member) = disk_member_with(
            2,
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
            Some(Devid::new(2)),
            None,
        );
        m.insert(canonical_luks_uuid(2), member)
            .expect("insert disk2");
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
            confirm: RecordingConfirm::new(),
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
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
            confirm: RecordingConfirm::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Intent: pin the shared raw-device-usage fixture builder to btrfs-progs'
    //   missing-device and whitespace shape.
    // Why it exists: downstream parser tests tolerate path text and whitespace,
    //   so only this exact assertion protects fixture fidelity.
    // Scenario: fixture authors need one canonical live + missing sample that
    //   cannot drift back to the stale older missing-device rendering.
    fn device_usage_raw_body_renders_canonical_live_and_missing_devices() {
        let body = device_usage_raw_body(&[
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk1",
                1,
                1_073_741_824,
                &[
                    ("Data", "RAID1", 52_428_800),
                    ("Metadata", "DUP", 10_485_760),
                ],
                1_010_794_496,
            ),
            DeviceUsageSpec::missing(3, &[("Data", "RAID1", 67_108_864)], 0),
        ]);

        assert_eq!(
            body,
            "/dev/mapper/braid-disk1, ID: 1\n\
             \x20  Device size:         1073741824\n\
             \x20  Device slack:                 0\n\
             \x20  Data,RAID1:            52428800\n\
             \x20  Metadata,DUP:          10485760\n\
             \x20  Unallocated:         1010794496\n\n\
             <missing disk>, ID: 3\n\
             \x20  Device size:                  0\n\
             \x20  Device slack:                 0\n\
             \x20  Data,RAID1:            67108864\n\
             \x20  Unallocated:                  0\n\n"
        );
    }

    // Intent: pin both `btrfs device remove` failure-stderr builders byte-for-byte
    //   to btrfs-progs' two arms -- by-id (`devid <n>`, no quotes) and by-path
    //   (`device '<path>'`, quoted) -- per `reference/btrfs-progs/cmds/device.c`.
    // Why it exists: the only consumer, `device_remove_error`, keys solely on the
    //   `"unable to go below"` substring (`pool.rs`), so the prefix shape is NOT
    //   load-bearing for behavior -- which is exactly why per-fixture literals had
    //   already drifted into the wrong arm. This pin is the only guard on the shape.
    // Scenario: a fixture author writes a remove-failure stderr by hand and reaches
    //   for the wrong arm (a quoted path for a by-devid removal, say); the builders
    //   make the correct shape the only one on offer, and this test locks them.
    #[test]
    fn btrfs_remove_error_builders_render_canonical_devid_and_path_arms() {
        assert_eq!(
            btrfs_remove_devid_error(3, "unable to go below three devices on raid1c3"),
            "ERROR: error removing devid 3: unable to go below three devices on raid1c3"
        );
        assert_eq!(
            btrfs_remove_path_error("/dev/mapper/braid-disk2", "No space left on device"),
            "ERROR: error removing device '/dev/mapper/braid-disk2': No space left on device"
        );
    }

    // Intent: reject any inline `btrfs device remove` failure-stderr literal at a
    //   call site, forcing every such fixture through btrfs_remove_devid_error /
    //   btrfs_remove_path_error.
    // Why it exists: the output pin above protects only the builders; nothing else
    //   stops a future author from hardcoding `stderr: "ERROR: error removing
    //   device ...".into()` at a call site, and behavioral tests would not catch it
    //   (they key on `"unable to go below"`, not the prefix). This source scan is
    //   the enforcement that closes that gap and keeps all fixtures honest.
    // Scenario: someone adds a new remove-failure test and pastes a raw stderr
    //   string instead of calling the builder; this test fails and names the file.
    //
    // Limitation: the scan list is explicit. A future command that adds
    //   device-remove failure fixtures in a NEW module must be added here -- a
    //   `cli/src/**` glob is not available to a unit test without a build script,
    //   and the explicit list is the simple, fail-loud choice (a moved/renamed file
    //   breaks `include_str!` at compile time). `shared.rs` is deliberately NOT
    //   scanned, so the builder bodies and this test's own needle never self-trip;
    //   `include_str!` resolves relative to this file (`cli/src/test_fixtures/`), so
    //   the call-site files are one dir up.
    #[test]
    fn no_inline_btrfs_remove_failure_literals_at_call_sites() {
        for (name, src) in [
            ("pool.rs", include_str!("../pool.rs")),
            ("remove.rs", include_str!("../remove.rs")),
            ("remove_missing.rs", include_str!("../remove_missing.rs")),
        ] {
            assert!(
                !src.contains("ERROR: error removing devi"),
                "{name}: inline `btrfs device remove` failure literal found -- route it \
                 through btrfs_remove_devid_error / btrfs_remove_path_error in \
                 test_fixtures::shared instead of hardcoding the stderr shape"
            );
        }
    }

    // Intent: canonical_luks_uuid(n) yields the exact repeated-digit literal for
    //   each disk index (1/2/3 -> 11111111-.../22222222-.../33333333-...) the
    //   inline fixtures used before this change.
    // Why it exists: step 2 routes BOTH the membership map (luks_uuid_for_disk_name)
    //   and the live-probe map (luks_uuid_for_device) through this one generator, so
    //   a wrong generator corrupts both sides in lockstep with no cross-check; this
    //   pins the output byte-for-byte against the literals it replaces.
    // Scenario: a refactor tweaks a segment length or the digit and silently shifts
    //   every canonical fixture UUID; this byte-for-byte tripwire fails closed.
    #[test]
    fn canonical_luks_uuid_pins_repeated_digit_literals() {
        assert_eq!(
            canonical_luks_uuid(1).as_str(),
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(
            canonical_luks_uuid(2).as_str(),
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(
            canonical_luks_uuid(3).as_str(),
            "33333333-3333-3333-3333-333333333333"
        );
    }

    // Intent: canonical_luks_uuid(0) panics instead of silently returning the nil
    //   UUID (00000000-...).
    // Why it exists: n == 0 builds the nil UUID, which aliases an "absent/zero"
    //   identity and defeats the fail-closed 1..=15 domain guard; this pins that
    //   guard so the n=0 trap cannot regress.
    // Scenario: a caller passes a 0-based disk index by mistake; the generator must
    //   fail closed, not mint a nil-UUID pool member.
    #[test]
    #[should_panic(expected = "canonical disk index must be 1..=15")]
    fn canonical_luks_uuid_rejects_disk_index_zero() {
        // n == 0 silently built the nil UUID before the guard -- the alias trap.
        let _ = canonical_luks_uuid(0);
    }
}
