//! Add-scope fixtures: `AddTopology`, `AddStatefulPool` /
//! `AddPoolHandle` / `AddDynFs` lifecycle topology, `AddPlanTopology`
//! plan-boundary topology, `AddParamsBuilder`, and the add-only
//! `PoolFixture` constructors.

use super::shared::{PoolFixture, mock_ok};
use crate::add::AddParams;
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::inhibit::RecordingInhibitor;
use crate::luks::{PassphraseReader, RealTty};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, DiskName, LuksUuid};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

impl PoolFixture {
    /// pool.json: disk1 only, no devid pinned. Models the canonical
    /// live "one-disk pool with returning braid-disk2" scaffold seeded
    /// by add.rs's `add_test_setup`. Distinct from `one_live_only`,
    /// which pins devid=1 for the absent-old-name typo scenario in
    /// replace tests.
    #[allow(dead_code)]
    pub(crate) fn live_one_disk() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        let uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111")
            .expect("canonical fixture UUID");
        m.insert(
            uuid,
            DiskMember {
                name: DiskName::parse("disk1").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                devid: None,
                added_at: None,
            },
        )
        .expect("fixture insert");
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

    /// Start an `AddParamsBuilder` whose defaults match the most common
    /// add-test shape: yes=true, dry_run=false, passphrase from fixture
    /// pass_path, progress=Off, no extra LUKS opts, no enrollment
    /// keyfile, passphrase_reader=&RealTty. `disk_specs` is required and
    /// caller-owned -- the test holds the slice in a `let` so its borrow
    /// outlives the builder.
    #[allow(dead_code)]
    pub(crate) fn add_params<'a>(&'a self, disk_specs: &'a [String]) -> AddParamsBuilder<'a> {
        AddParamsBuilder {
            config_path: &self.config_path,
            disk_specs,
            dry_run: false,
            yes: true,
            passphrase_stdin: false,
            passphrase_file: Some(self.pass_path.as_path()),
            enroll_key_file: None,
            luks_format_extra_opts: &[],
            progress: ProgressOutput::Off,
            paths: &self.paths,
            inhibitor: &self.inhibitor,
            passphrase_reader: &RealTty,
        }
    }
}

/// Per-test `AddParams` builder. Defaults match the most common test
/// shape (yes=true, dry_run=false, passphrase from fixture pass_path,
/// progress=Off, no extra LUKS opts, no enrollment keyfile,
/// passphrase_reader=&RealTty). Every field is overridable via fluent
/// setters so corner-case tests can round-trip identical `AddParams`
/// values to what they constructed inline pre-migration.
///
/// `disk_specs` is caller-owned: the test holds the slice in a `let` so
/// its borrow outlives the builder. This matches the production
/// `AddParams::disk_specs: &'a [String]` shape exactly.
#[allow(dead_code)]
pub(crate) struct AddParamsBuilder<'a> {
    config_path: &'a Path,
    disk_specs: &'a [String],
    dry_run: bool,
    yes: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&'a Path>,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
    progress: ProgressOutput,
    paths: &'a StatePaths,
    inhibitor: &'a RecordingInhibitor,
    passphrase_reader: &'a dyn PassphraseReader,
}

#[allow(dead_code)]
impl<'a> AddParamsBuilder<'a> {
    pub(crate) fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
    pub(crate) fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }
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
    pub(crate) fn progress(mut self, p: ProgressOutput) -> Self {
        self.progress = p;
        self
    }
    pub(crate) fn passphrase_reader(mut self, reader: &'a dyn PassphraseReader) -> Self {
        self.passphrase_reader = reader;
        self
    }
    pub(crate) fn build(self) -> AddParams<'a> {
        AddParams {
            config_path: self.config_path,
            disk_specs: self.disk_specs,
            dry_run: self.dry_run,
            yes: self.yes,
            passphrase_stdin: self.passphrase_stdin,
            passphrase_file: self.passphrase_file,
            enroll_key_file: self.enroll_key_file,
            luks_format_extra_opts: self.luks_format_extra_opts,
            progress: self.progress,
            paths: self.paths,
            sleep_inhibitor: self.inhibitor,
            passphrase_reader: self.passphrase_reader,
        }
    }
}

// ---------------------------------------------------------------------------
// Add topology shared constants
// ---------------------------------------------------------------------------

/// Canonical pool FSID used by every add fixture's `BtrfsFilesystemShow`
/// body. Matches the `POOL_FSID` constant inlined in add.rs's tests so
/// migrated tests retain byte-identical pool-show output.
pub(crate) const ADD_POOL_FSID: &str = "cc86845b-aec3-408e-bef5-553affc1f2b1";

// ---------------------------------------------------------------------------
// AddTopology
// ---------------------------------------------------------------------------

/// Static one-disk pool topology installer for `add` tests.
///
/// Resolves the canonical "live one-disk pool with returning braid-disk2"
/// surface via one `with_handler` closure: BtrfsFilesystemShow,
/// CryptsetupStatus (braid-disk{1,2} active), CryptsetupLuksUuid (mapper
/// underlying `/dev/vd{b,c}` plus by-id `virtio-disk{1,2}`),
/// BtrfsBalanceStatus, CryptsetupLuksDumpText (LUKS2 with braid-disk2
/// label), BtrfsFilesystemShowTarget (FSID match or "not a valid btrfs"
/// per `no_btrfs_superblock` knob), BtrfsDeviceAdd (success), and
/// CryptsetupTestPassphrase (success).
///
/// Failure-injection knobs that the legacy `AddTestRunner` carried as
/// boolean fields (`fail_device_add`, etc.) become per-test
/// `with_handler` overrides registered AFTER `install`; reverse-order
/// dispatch ensures the per-test handler shadows the topology default.
#[allow(dead_code)]
pub(crate) struct AddTopology {
    disk_in_pool: bool,
    no_btrfs_superblock: bool,
}

#[allow(dead_code)]
impl AddTopology {
    /// Default shape: live one-disk pool (braid-disk1), braid-disk2 is a
    /// returning LUKS-labeled disk that classifies as recoverable.
    pub(crate) fn live_one_disk() -> Self {
        Self {
            disk_in_pool: false,
            no_btrfs_superblock: false,
        }
    }

    /// When true, BtrfsFilesystemShow reports both braid-disk1 and
    /// braid-disk2 as live members (Total devices 2). Drives the
    /// already-in-pool note-only success path.
    pub(crate) fn with_disk_in_pool(mut self, in_pool: bool) -> Self {
        self.disk_in_pool = in_pool;
        self
    }

    /// When true, BtrfsFilesystemShowTarget for the candidate's mapper
    /// returns "not a valid btrfs filesystem" (BraidLabeledNoBtrfs).
    pub(crate) fn with_no_btrfs_superblock(mut self, no_super: bool) -> Self {
        self.no_btrfs_superblock = no_super;
        self
    }

    /// Push one closure handler resolving the canonical surface. Reverse-order
    /// dispatch lets per-test code register additional `with_handler`
    /// overrides afterwards (e.g. inject a btrfs device-add failure).
    pub(crate) fn install(self, runner: MockRunner) -> MockRunner {
        let disk_in_pool = self.disk_in_pool;
        let no_btrfs_superblock = self.no_btrfs_superblock;

        runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                let disk2_line = if disk_in_pool {
                    "\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n"
                } else {
                    ""
                };
                let total = if disk_in_pool { 2 } else { 1 };
                Some(Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    &format!(
                        "Label: none  uuid: {ADD_POOL_FSID}\n\
                         \tTotal devices {total} FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         {disk2_line}"
                    ),
                )))
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                let underlying = match mapper.as_str() {
                    "braid-disk1" => "/dev/vdb",
                    "braid-disk2" => "/dev/vdc",
                    _ => return None,
                };
                Some(Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  \
                         type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                    ),
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                let uuid = match device.as_str() {
                    "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                        "11111111-1111-1111-1111-111111111111"
                    }
                    "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                        "22222222-2222-2222-2222-222222222222"
                    }
                    _ => return None,
                };
                Some(Ok(mock_ok(
                    &format!("cryptsetup luksUUID {device}"),
                    &format!("{uuid}\n"),
                )))
            }
            CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ))),
            CmdRequest::CryptsetupLuksDumpText { .. } => Some(Ok(mock_ok(
                "cryptsetup luksDump",
                "LUKS header information\n\
                 Version:       \t2\n\
                 Label:         \tbraid-disk2\n\
                 Subsystem:     \t(no subsystem)\n",
            ))),
            CmdRequest::BtrfsFilesystemShowTarget { target } => {
                if no_btrfs_superblock {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("btrfs filesystem show {target}"),
                        stdout: String::new(),
                        stderr: format!("ERROR: not a valid btrfs filesystem on {target}"),
                        exit_status: 1,
                    }))
                } else {
                    Some(Ok(mock_ok(
                        &format!("btrfs filesystem show {target}"),
                        &format!(
                            "Label: none  uuid: {ADD_POOL_FSID}\n\
                             \tTotal devices 1 FS bytes used 16.17MiB\n\
                             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n"
                        ),
                    )))
                }
            }
            CmdRequest::BtrfsDeviceAdd { .. } => {
                Some(Ok(mock_ok("btrfs device add", "")))
            }
            CmdRequest::CryptsetupTestPassphrase { device } => Some(Ok(mock_ok(
                &format!("cryptsetup open --test-passphrase {device}"),
                "",
            ))),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// AddStatefulPool / AddPoolHandle / AddDynFs
// ---------------------------------------------------------------------------

/// Mode for `AddStatefulPool::install`. `Live` starts mounted with
/// braid-disk1 already opened; `Bootstrap` starts unmounted with no
/// mappers open. Distinguishing names mirror the production scenario
/// rather than the internal flags so tests inherit the production
/// mental model from the constructor.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum AddPoolMode {
    Live,
    Bootstrap,
}

/// Stateful installer modelling the bootstrap+live `cmd_add` mutation
/// lifecycle: `mounted` flips on `Mount`, `opened` grows on
/// `CryptsetupLuksOpen`, `added` grows on `BtrfsDeviceAdd`. The shared
/// `Arc`s (`AtomicBool` + `Mutex<Vec<String>>`) live in `AddPoolHandle`
/// so per-test handlers can read them in conditional failure-injection
/// closures (e.g. "fail BtrfsFilesystemShow once added is non-empty").
///
/// Backed by one `with_handler` closure resolving the full bootstrap+live
/// surface. Per-test failure-injection closures register AFTER `install`
/// and shadow the topology default via reverse-order dispatch.
#[allow(dead_code)]
pub(crate) struct AddStatefulPool {
    mode: AddPoolMode,
    disk2_devid: u64,
    omit_new_mapper_from_probe: bool,
}

/// Handle returned by `AddStatefulPool::install`. Hands back the shared
/// `Arc`s the topology closure captured so per-test code can both
/// observe state (`added_mappers`, `mounted.load(...)`) and clone the
/// `Arc`s into conditional failure-injection handlers.
///
/// The three `Arc`s are `pub(crate)` (not just `mounted`) so handlers
/// can clone them directly into closures without going through accessor
/// methods -- the canonical pattern is `let mounted = handle.mounted.clone();`
/// inside the test body.
#[allow(dead_code)]
pub(crate) struct AddPoolHandle {
    pub(crate) mounted: Arc<AtomicBool>,
    pub(crate) added: Arc<Mutex<Vec<String>>>,
    pub(crate) opened: Arc<Mutex<Vec<String>>>,
}

#[allow(dead_code)]
impl AddStatefulPool {
    /// Live one-disk pool: mounted=true, opened=[braid-disk1], added=[].
    /// disk2_devid defaults to 2.
    pub(crate) fn live_one_disk() -> Self {
        Self {
            mode: AddPoolMode::Live,
            disk2_devid: 2,
            omit_new_mapper_from_probe: false,
        }
    }

    /// Fresh bootstrap: mounted=false, opened=[], added=[].
    /// disk2_devid defaults to 2.
    pub(crate) fn fresh_bootstrap() -> Self {
        Self {
            mode: AddPoolMode::Bootstrap,
            disk2_devid: 2,
            omit_new_mapper_from_probe: false,
        }
    }

    /// Override the devid btrfs reports for braid-disk2 in
    /// BtrfsFilesystemShow output. Models the live-add ghost-cleanup
    /// case where btrfs assigns a non-sequential devid (e.g. 7) to a
    /// returning mapper.
    pub(crate) fn with_disk2_devid(mut self, devid: u64) -> Self {
        self.disk2_devid = devid;
        self
    }

    /// When set, BtrfsFilesystemShow does NOT include the freshly-added
    /// mapper(s) in the post-add probe. Models the
    /// post_add_probe_uncertainty arm where btrfs has not yet observed
    /// the new device.
    pub(crate) fn with_new_mapper_omitted_from_probe(mut self) -> Self {
        self.omit_new_mapper_from_probe = true;
        self
    }

    /// Install the topology closure on `runner` and return the handle.
    /// The closure captures cloned `Arc`s so observation and conditional
    /// failure injection in per-test handlers see the same state the
    /// topology mutates.
    pub(crate) fn install(self, runner: MockRunner) -> (MockRunner, AddPoolHandle) {
        let (initial_mounted, initial_opened) = match self.mode {
            AddPoolMode::Live => (true, vec!["braid-disk1".to_owned()]),
            AddPoolMode::Bootstrap => (false, Vec::new()),
        };
        let mounted = Arc::new(AtomicBool::new(initial_mounted));
        let opened = Arc::new(Mutex::new(initial_opened));
        let added: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let disk2_devid = self.disk2_devid;
        let omit_new_mapper_from_probe = self.omit_new_mapper_from_probe;

        let h_mounted = Arc::clone(&mounted);
        let h_opened = Arc::clone(&opened);
        let h_added = Arc::clone(&added);

        let runner = runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                let mut mappers = vec!["braid-disk1".to_owned()];
                if !omit_new_mapper_from_probe {
                    mappers.extend(h_added.lock().unwrap().iter().cloned());
                }
                let mut body = format!(
                    "Label: none  uuid: {ADD_POOL_FSID}\n\
                     \tTotal devices {} FS bytes used 16.17MiB\n",
                    mappers.len()
                );
                for mapper in &mappers {
                    let devid = mapper_devid(mapper, disk2_devid);
                    body.push_str(&format!(
                        "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{mapper}\n"
                    ));
                }
                Some(Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    &body,
                )))
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                if h_opened.lock().unwrap().iter().any(|m| m == mapper) {
                    let underlying = mapper_underlying(mapper);
                    Some(Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  \
                             type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                        ),
                    )))
                } else {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup status {mapper}"),
                        stdout: String::new(),
                        stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                        exit_status: 4,
                    }))
                }
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                if let Some(uuid) = luks_uuid_for_underlying(device) {
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    )))
                } else {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
            }
            CmdRequest::CryptsetupTestPassphrase { device } => Some(Ok(mock_ok(
                &format!("cryptsetup open --test-passphrase {device}"),
                "",
            ))),
            CmdRequest::CryptsetupLuksFormat { device, .. } => Some(Ok(mock_ok(
                &format!("cryptsetup luksFormat {device}"),
                "",
            ))),
            CmdRequest::CryptsetupLuksAddKeyFile { device, .. } => Some(Ok(mock_ok(
                &format!("cryptsetup luksAddKey {device}"),
                "",
            ))),
            CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => Some(Ok(mock_ok(
                &format!("cryptsetup luksHeaderBackup {device}"),
                "",
            ))),
            CmdRequest::CryptsetupLuksOpen { device, mapper } => {
                h_opened.lock().unwrap().push(mapper.clone());
                Some(Ok(mock_ok(
                    &format!("cryptsetup open --type luks {device} {mapper}"),
                    "",
                )))
            }
            CmdRequest::BtrfsFilesystemShowTarget { target } => Some(Ok(RawCommandOutput {
                cmd: format!("btrfs filesystem show {target}"),
                stdout: String::new(),
                stderr: format!("ERROR: not a valid btrfs filesystem on {target}"),
                exit_status: 1,
            })),
            CmdRequest::MkfsBtrfs { device } => Some(Ok(mock_ok(
                &format!("mkfs.btrfs {device}"),
                "",
            ))),
            CmdRequest::MkfsBtrfsRaid1 { devices } => Some(Ok(mock_ok(
                &format!("mkfs.btrfs {}", devices.join(" ")),
                "",
            ))),
            CmdRequest::Mount { device, .. } => {
                h_mounted.store(true, Ordering::SeqCst);
                Some(Ok(mock_ok(&format!("mount {device}"), "")))
            }
            CmdRequest::BtrfsDeviceAdd { device, .. } => {
                let mapper = device
                    .strip_prefix("/dev/mapper/")
                    .expect("test device-add path must be mapper")
                    .to_owned();
                h_added.lock().unwrap().push(mapper);
                Some(Ok(mock_ok(&format!("btrfs device add {device}"), "")))
            }
            CmdRequest::BtrfsBalanceRaid1 { .. } => Some(Ok(mock_ok("btrfs balance start", ""))),
            CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ))),
            _ => None,
        });

        (
            runner,
            AddPoolHandle {
                mounted,
                added,
                opened,
            },
        )
    }
}

#[allow(dead_code)]
impl AddPoolHandle {
    /// Snapshot the mappers `BtrfsDeviceAdd` has been invoked on, in
    /// invocation order. Replaces `AddFullPathRunner::added_mappers`.
    pub(crate) fn added_mappers(&self) -> Vec<String> {
        self.added.lock().unwrap().clone()
    }

    /// Build an `AddDynFs` whose `mountinfo` flips with the same
    /// `Arc<AtomicBool>` the topology mutates on `Mount`. The returned
    /// fs is the one to pass to `cmd_add` -- using a static `MockFs`
    /// with a stateful pool would desync mountinfo from the actual mount
    /// state.
    pub(crate) fn fs(&self, paths: Vec<String>) -> AddDynFs {
        AddDynFs {
            paths,
            mounted: Arc::clone(&self.mounted),
        }
    }
}

/// `Filesystem` mock whose `/proc/self/mountinfo` body flips with a
/// shared `Arc<AtomicBool>`. Used with `AddStatefulPool` so the
/// bootstrap path's pre-mount mountinfo (rootfs only) and post-mount
/// mountinfo (/mnt/storage on btrfs) reflect the lifecycle the topology
/// drives.
#[allow(dead_code)]
pub(crate) struct AddDynFs {
    paths: Vec<String>,
    mounted: Arc<AtomicBool>,
}

impl Filesystem for AddDynFs {
    fn exists(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p == path)
    }
    fn is_block_device(&self, _path: &str) -> bool {
        false
    }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            if self.mounted.load(Ordering::SeqCst) {
                Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                )
            } else {
                Ok("26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".to_owned())
            }
        } else if path.ends_with("/exclusive_operation") {
            Ok("none\n".to_owned())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }
}

fn mapper_devid(mapper: &str, disk2_devid: u64) -> u64 {
    match mapper {
        "braid-disk1" => 1,
        "braid-disk2" => disk2_devid,
        "braid-disk3" => 3,
        other => panic!("unexpected mapper for devid mapping: {other}"),
    }
}

fn mapper_underlying(mapper: &str) -> &'static str {
    match mapper {
        "braid-disk1" => "/dev/vdb",
        "braid-disk2" => "/dev/vdc",
        "braid-disk3" => "/dev/vdd",
        other => panic!("unexpected mapper for underlying mapping: {other}"),
    }
}

fn luks_uuid_for_underlying(device: &str) -> Option<&'static str> {
    match device {
        "/dev/vdb" => Some("11111111-1111-1111-1111-111111111111"),
        "/dev/vdc" => Some("22222222-2222-2222-2222-222222222222"),
        "/dev/vdd" => Some("33333333-3333-3333-3333-333333333333"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// AddPlanTopology / AddPlanKeyfileProbe
// ---------------------------------------------------------------------------

/// Per-existing-device keyfile-probe response shape for
/// `AddPlanTopology`. Each entry indexes into the synthesized N-disk
/// pool's existing-disk LUKS dump output, driving
/// `probe_pool_keyfile_enrollment`'s asymmetry / failure / clean
/// branches.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum AddPlanKeyfileProbe {
    Empty,
    Occupied,
    Failure,
}

/// `plan_add` boundary topology installer. Models a live N-disk pool
/// (N = `keyfile_probes.len()`) with optional missing-device
/// placeholders and parameterised LUKS-dump responses for the existing
/// disks' underlying devices.
///
/// The candidate add-disk (e.g. `/dev/disk/by-id/virtio-disk{N+1}`)
/// falls through `CryptsetupLuksUuid` to "not a valid LUKS device", so
/// `probe_config_disk` classifies it as `PresentNotLuks`. Without that
/// fallback, the candidate probe would hit `MissingMock` and break
/// every test that adds a fresh disk against a live pool.
#[allow(dead_code)]
pub(crate) struct AddPlanTopology {
    missing_count: u64,
    keyfile_probes: Vec<AddPlanKeyfileProbe>,
}

#[allow(dead_code)]
impl AddPlanTopology {
    /// One-disk pool, one Empty keyfile probe, no missing devices.
    /// Matches the legacy `AddPlanTestRunner::new()` default.
    pub(crate) fn new() -> Self {
        Self {
            missing_count: 0,
            keyfile_probes: vec![AddPlanKeyfileProbe::Empty],
        }
    }

    /// Synthesize `n` MISSING devid placeholders alongside the real
    /// devices so `probe_pool`'s missing-device arithmetic
    /// (`show.total_devices - devices.len()`) reports `n` missing.
    pub(crate) fn with_missing(mut self, n: u64) -> Self {
        self.missing_count = n;
        self
    }

    /// Mark the single existing disk's keyfile probe as Occupied.
    /// Drives the keyfile-asymmetry warning path.
    pub(crate) fn with_keyfile_occupied(mut self) -> Self {
        self.keyfile_probes = vec![AddPlanKeyfileProbe::Occupied];
        self
    }

    /// Mark the single existing disk's keyfile probe as Failure
    /// (luksDump exit != 0). Drives the keyfile-uncertainty warning
    /// path.
    pub(crate) fn with_keyfile_probe_failure(mut self) -> Self {
        self.keyfile_probes = vec![AddPlanKeyfileProbe::Failure];
        self
    }

    /// Override the per-existing-disk keyfile-probe responses. Length
    /// determines the pool size.
    pub(crate) fn with_keyfile_probes(mut self, probes: Vec<AddPlanKeyfileProbe>) -> Self {
        self.keyfile_probes = probes;
        self
    }

    /// Push one closure handler resolving the canonical plan_add
    /// surface. Per-test code can register additional handlers
    /// afterwards (e.g. inject UPS=OB on `UpscQuery`).
    pub(crate) fn install(self, runner: MockRunner) -> MockRunner {
        let missing_count = self.missing_count;
        let keyfile_probes = self.keyfile_probes;

        runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                let real_devices = keyfile_probes.len() as u64;
                let total = real_devices + missing_count;
                let mut out = format!(
                    "Label: none  uuid: {ADD_POOL_FSID}\n\
                     \tTotal devices {total} FS bytes used 16.17MiB\n"
                );
                for i in 0..keyfile_probes.len() {
                    let devid = i + 1;
                    out.push_str(&format!(
                        "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk{devid}\n"
                    ));
                }
                for i in 0..missing_count {
                    let devid = real_devices + 1 + i;
                    out.push_str(&format!("\tdevid    {devid} size 0 used 0 path MISSING\n"));
                }
                Some(Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    &out,
                )))
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                let suffix = mapper.strip_prefix("braid-disk")?;
                let index = suffix.parse::<usize>().ok()?.checked_sub(1)?;
                if index >= keyfile_probes.len() {
                    return None;
                }
                let underlying = pool_underlying_for_index(index);
                Some(Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  \
                         type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                    ),
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                if let Some(index) = keyfile_probes.iter().enumerate().find_map(|(idx, _)| {
                    let disk = idx + 1;
                    let underlying = pool_underlying_for_index(idx);
                    let by_id = format!("/dev/disk/by-id/virtio-disk{disk}");
                    (device == &underlying || device == &by_id).then_some(idx)
                }) {
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("11111111-1111-1111-1111-11111111111{index}\n"),
                    )))
                } else {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
            }
            CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ))),
            CmdRequest::CryptsetupLuksDump { device } => {
                let (index, probe) = keyfile_probes
                    .iter()
                    .enumerate()
                    .find(|(idx, _)| device == &pool_underlying_for_index(*idx))?;
                match probe {
                    AddPlanKeyfileProbe::Empty => Some(Ok(mock_ok(
                        "cryptsetup luksDump --dump-json-metadata",
                        r#"{"keyslots":{"0":{"type":"luks2"}}}"#,
                    ))),
                    AddPlanKeyfileProbe::Occupied => Some(Ok(mock_ok(
                        "cryptsetup luksDump --dump-json-metadata",
                        r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                    ))),
                    AddPlanKeyfileProbe::Failure => Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                        stdout: String::new(),
                        stderr: format!(
                            "forced luksDump failure on existing disk {}",
                            index + 1
                        ),
                        exit_status: 5,
                    })),
                }
            }
            _ => None,
        })
    }
}

fn pool_underlying_for_index(index: usize) -> String {
    format!("/dev/vd{}", (b'b' + index as u8) as char)
}
