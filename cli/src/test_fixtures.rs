//! Test-only shared fixtures for `replace` (and, in follow-ups, `add`,
//! `remove`, `remove_missing`, `recover`, `doctor`).
//!
//! These fixtures consolidate the per-test scaffolding that previously
//! lived as one-off `*Runner` structs and inline `tempdir + config + pass +
//! membership` setups. The split is:
//!
//!   * `MockFs` -- generic `Filesystem` mock with the canonical
//!     `/proc/self/mountinfo` body and an optional sysfs override.
//!   * `ReplacementPool` -- canonical pool-topology mock-handler
//!     installer (mapper -> dev, dev -> uuid, btrfs filesystem show /
//!     usage with state flipping on `replace_done`, plus the boring
//!     preflight surface).
//!   * `PoolFixture` -- bundled tempdirs + `StatePaths` + config +
//!     passphrase + `RecordingInhibitor`.
//!   * `ReplaceParamsBuilder` -- per-test builder over `ReplaceParams`
//!     defaults.

use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::progress::ProgressOutput;
use crate::replace::ReplaceParams;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
// ReplacementPool
// ---------------------------------------------------------------------------

const PRE_SHOW_TWO_HEALTHY: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n";

const PRE_SHOW_ONE_LIVE_MISSING: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \t*** Some devices missing\n";

const POST_SHOW_DISK1_DISK3: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";

const PRE_USAGE_RAW_TWO_HEALTHY: &str = "/dev/mapper/braid-disk1, ID: 1\n\
     \tDevice size:           520093696\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:            50331648\n\n\
     /dev/mapper/braid-disk2, ID: 2\n\
     \tDevice size:           520093696\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:            50331648\n\n";

const PRE_USAGE_RAW_ONE_LIVE_MISSING: &str = "/dev/mapper/braid-disk1, ID: 1\n\
     \tDevice size:           520093696\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:            50331648\n\n\
     <missing disk>, ID: 2\n\
     \tDevice size:                  0\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:                  0\n\n";

const POST_USAGE_RAW_DISK1_DISK3: &str = "/dev/mapper/braid-disk1, ID: 1\n\
     \tDevice size:           520093696\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:            50331648\n\n\
     /dev/mapper/braid-disk3, ID: 2\n\
     \tDevice size:           520093696\n\
     \tDevice slack:                  0\n\
     \tData,RAID1:            469762048\n\
     \tUnallocated:            50331648\n\n";

/// Canonical pool-topology mock-handler installer.
///
/// One closure registered via `MockRunner::with_handler` resolves the full
/// preflight + replace surface from the topology maps and a `replace_done`
/// `AtomicBool` flag that flips state-dependent outputs (`BtrfsFilesystemShow`
/// and `BtrfsDeviceUsageRaw`) from pre- to post-replace shape.
pub(crate) struct ReplacementPool {
    pre_show: &'static str,
    post_show: &'static str,
    pre_usage_raw: &'static str,
    post_usage_raw: &'static str,
    mapper_to_dev: HashMap<&'static str, &'static str>,
    dev_to_uuid: HashMap<&'static str, &'static str>,
    closed_mappers: HashSet<&'static str>,
}

impl ReplacementPool {
    fn canonical_mapper_to_dev() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("braid-disk1", "/dev/vdb"),
            ("braid-disk2", "/dev/vdc"),
            ("braid-disk3", "/dev/vdd"),
        ])
    }

    fn canonical_dev_to_uuid() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("/dev/vdb", "11111111-1111-1111-1111-111111111111"),
            (
                "/dev/disk/by-id/virtio-disk1",
                "11111111-1111-1111-1111-111111111111",
            ),
            ("/dev/vdc", "22222222-2222-2222-2222-222222222222"),
            (
                "/dev/disk/by-id/virtio-disk2",
                "22222222-2222-2222-2222-222222222222",
            ),
            ("/dev/vdd", "33333333-3333-3333-3333-333333333333"),
            (
                "/dev/disk/by-id/virtio-disk3",
                "33333333-3333-3333-3333-333333333333",
            ),
        ])
    }

    /// Live disk1 + live disk2; replace flips topology to disk1 + disk3.
    pub(crate) fn two_disk_healthy() -> Self {
        Self {
            pre_show: PRE_SHOW_TWO_HEALTHY,
            post_show: POST_SHOW_DISK1_DISK3,
            pre_usage_raw: PRE_USAGE_RAW_TWO_HEALTHY,
            post_usage_raw: POST_USAGE_RAW_DISK1_DISK3,
            mapper_to_dev: Self::canonical_mapper_to_dev(),
            dev_to_uuid: Self::canonical_dev_to_uuid(),
            closed_mappers: HashSet::new(),
        }
    }

    /// Live disk1 + missing devid 2; replace flips topology to disk1 + disk3.
    pub(crate) fn one_live_one_missing() -> Self {
        Self {
            pre_show: PRE_SHOW_ONE_LIVE_MISSING,
            post_show: POST_SHOW_DISK1_DISK3,
            pre_usage_raw: PRE_USAGE_RAW_ONE_LIVE_MISSING,
            post_usage_raw: POST_USAGE_RAW_DISK1_DISK3,
            mapper_to_dev: Self::canonical_mapper_to_dev(),
            dev_to_uuid: Self::canonical_dev_to_uuid(),
            closed_mappers: HashSet::new(),
        }
    }

    /// Mark `mapper` as inactive (cryptsetup status reports `inactive`,
    /// exit_status=4) instead of the default active+LUKS2 reply. Use for
    /// the closed-LUKS / fresh-disk variants.
    pub(crate) fn with_mapper_closed(mut self, mapper: &'static str) -> Self {
        self.closed_mappers.insert(mapper);
        self
    }

    /// Register a single closure handler on `runner` that resolves the
    /// canonical preflight + replace surface from the maps + replace_done
    /// flag. Per-test code can call `with_handler` again afterwards to
    /// override specific request shapes (e.g. inject a btrfs replace
    /// failure or shadow disk3's LUKS UUID). Reverse-order dispatch in
    /// `MockRunner` ensures those overrides win.
    pub(crate) fn install(self, runner: MockRunner, replace_done: Arc<AtomicBool>) -> MockRunner {
        let pre_show = self.pre_show;
        let post_show = self.post_show;
        let pre_usage_raw = self.pre_usage_raw;
        let post_usage_raw = self.post_usage_raw;
        let mapper_to_dev = self.mapper_to_dev;
        let dev_to_uuid = self.dev_to_uuid;
        let closed_mappers = self.closed_mappers;

        runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                let body = if replace_done.load(Ordering::Relaxed) {
                    post_show
                } else {
                    pre_show
                };
                Some(Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    body,
                )))
            }
            CmdRequest::BtrfsDeviceUsageRaw { .. } => {
                let body = if replace_done.load(Ordering::Relaxed) {
                    post_usage_raw
                } else {
                    pre_usage_raw
                };
                Some(Ok(mock_ok("btrfs device usage --raw", body)))
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                if closed_mappers.contains(mapper.as_str()) {
                    return Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup status {mapper}"),
                        stdout: String::new(),
                        stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                        exit_status: 4,
                    }));
                }
                let dev = (*mapper_to_dev.get(mapper.as_str())?).to_owned();
                Some(Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                    ),
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                let uuid = (*dev_to_uuid.get(device.as_str())?).to_owned();
                Some(Ok(mock_ok(
                    &format!("cryptsetup luksUUID {device}"),
                    &format!("{uuid}\n"),
                )))
            }
            CmdRequest::CryptsetupLuksDumpText { device } => Some(Ok(mock_ok(
                &format!("cryptsetup luksDump {device}"),
                "LUKS header information\nVersion:       \t2\n",
            ))),
            CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ))),
            CmdRequest::BtrfsDeviceStatsJson { .. } => Some(Ok(mock_ok(
                "btrfs device stats",
                r#"{"device-stats": []}"#,
            ))),
            CmdRequest::CryptsetupTestPassphrase { device } => Some(Ok(mock_ok(
                &format!("cryptsetup open --test-passphrase {device}"),
                "",
            ))),
            _ => None,
        })
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
    _state_tmp: TempDir,
    pub(crate) paths: StatePaths,
    _config_tmp: TempDir,
    pub(crate) config_path: PathBuf,
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
}

impl PoolFixture {
    /// Build the temp directories + canonical config.json + passphrase
    /// file used by every constructor.
    fn empty_inner() -> (TempDir, StatePaths, TempDir, PathBuf, PathBuf) {
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

    /// pool.json: disk1 (devid=1) only -- absent-old-name typo scenario
    /// for `cmd_replace_missing_path_rejects_old_name_absent_from_membership`.
    /// btrfs still reports devid 2 missing; pool.json doesn't record it.
    pub(crate) fn one_live_only() -> Self {
        let (state_tmp, paths, config_tmp, config_path, pass_path) = Self::empty_inner();
        let mut m = PoolMembership::empty();
        let mut disk1 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into()));
        disk1.devid = Some(1);
        m.disks.insert("disk1".into(), disk1);
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

    /// Start a `ReplaceParamsBuilder` whose defaults match the most
    /// common test shape: `--old disk2 --new disk3=...`, yes=true,
    /// dry_run=false, passphrase from the fixture's pass file.
    pub(crate) fn replace_params(&self) -> ReplaceParamsBuilder<'_> {
        ReplaceParamsBuilder {
            old_name: "disk2",
            new_name: "disk3=/dev/disk/by-id/virtio-disk3",
            missing_id: None,
            dry_run: false,
            yes: true,
            passphrase_stdin: false,
            passphrase_file: Some(self.pass_path.as_path()),
            enroll_key_file: None,
            luks_format_extra_opts: &[],
            progress: ProgressOutput::Off,
            config_path: &self.config_path,
            paths: &self.paths,
            inhibitor: &self.inhibitor,
        }
    }
}

// ---------------------------------------------------------------------------
// ReplaceParamsBuilder
// ---------------------------------------------------------------------------

/// Per-test `ReplaceParams` builder. Defaults match the most common test
/// shape (yes=true, dry_run=false, passphrase from fixture, progress=Off,
/// no extra LUKS opts, missing_id=None, no enrollment keyfile). Every
/// field is overridable via fluent setters so corner-case tests can
/// round-trip identical `ReplaceParams` values to what they constructed
/// inline pre-migration.
pub(crate) struct ReplaceParamsBuilder<'a> {
    old_name: &'a str,
    new_name: &'a str,
    missing_id: Option<u64>,
    dry_run: bool,
    yes: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&'a Path>,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
    progress: ProgressOutput,
    config_path: &'a Path,
    paths: &'a StatePaths,
    inhibitor: &'a RecordingInhibitor,
}

impl<'a> ReplaceParamsBuilder<'a> {
    pub(crate) fn old(mut self, name: &'a str) -> Self {
        self.old_name = name;
        self
    }
    pub(crate) fn new(mut self, spec: &'a str) -> Self {
        self.new_name = spec;
        self
    }
    pub(crate) fn missing_id(mut self, id: Option<u64>) -> Self {
        self.missing_id = id;
        self
    }
    pub(crate) fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
    pub(crate) fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn passphrase_stdin(mut self, on: bool) -> Self {
        self.passphrase_stdin = on;
        self
    }
    pub(crate) fn passphrase_file(mut self, path: Option<&'a Path>) -> Self {
        self.passphrase_file = path;
        self
    }
    pub(crate) fn enroll_key_file(mut self, path: Option<&'a Path>) -> Self {
        self.enroll_key_file = path;
        self
    }
    pub(crate) fn luks_format_extra_opts(mut self, opts: &'a [String]) -> Self {
        self.luks_format_extra_opts = opts;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn progress(mut self, p: ProgressOutput) -> Self {
        self.progress = p;
        self
    }
    pub(crate) fn build(self) -> ReplaceParams<'a> {
        ReplaceParams {
            config_path: self.config_path,
            old_name: self.old_name,
            new_name: self.new_name,
            missing_id: self.missing_id,
            dry_run: self.dry_run,
            yes: self.yes,
            passphrase_stdin: self.passphrase_stdin,
            passphrase_file: self.passphrase_file,
            enroll_key_file: self.enroll_key_file,
            luks_format_extra_opts: self.luks_format_extra_opts,
            progress: self.progress,
            paths: self.paths,
            sleep_inhibitor: self.inhibitor,
        }
    }
}
