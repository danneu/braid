use std::thread;
use std::time::Duration;

use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{mapper_name, name_from_mapper, Config};
use crate::membership::PoolMembership;
use crate::preflight;
use crate::probe::{probe_fsid, Filesystem};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("device busy: {0}")]
    DeviceBusy(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

const CLOSE_RETRY_ATTEMPTS: u32 = 3;
const CLOSE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Abstraction over `thread::sleep` so unit tests can drive the retry
/// loop without paying real wall-clock time. The production path uses
/// `RealSleeper`; tests inject a noop or recording sleeper.
trait Sleeper {
    fn sleep(&self, duration: Duration);
}

struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Close a LUKS mapper, retrying up to 3 times if the error indicates the
/// device is busy. Non-busy errors fail immediately.
fn close_mapper_with_retry<R: CommandRunner, S: Sleeper>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
) -> Result<(), LockError> {
    for attempt in 1..=CLOSE_RETRY_ATTEMPTS {
        let result = runner.run(&CmdRequest::CryptsetupClose {
            mapper: mapper.to_owned(),
        })?;
        if result.exit_status == 0 {
            return Ok(());
        }
        let msg = format!(
            "cryptsetup close {} failed (exit {}): {}",
            mapper,
            result.exit_status,
            result.stderr.trim()
        );
        // cryptsetup close (lib/setup.c:5763-5811) returns -EBUSY for a held
        // mapper, translated to exit 5 by src/utils_tools.c translate_errno.
        // On the close path exit 5 is EBUSY-exclusive (no -EEXIST branch),
        // so matching exit status is wording- and locale-agnostic and
        // survives upstream phrasing drift.
        let is_busy = result.exit_status == 5;
        if !is_busy {
            return Err(LockError::Failed(msg));
        }
        if attempt == CLOSE_RETRY_ATTEMPTS {
            return Err(LockError::DeviceBusy(msg));
        }
        eprintln!(
            "[warn]  cryptsetup close {mapper} busy, retrying ({attempt}/{CLOSE_RETRY_ATTEMPTS})..."
        );
        sleeper.sleep(CLOSE_RETRY_DELAY);
    }
    unreachable!()
}

/// Enumerate braid-* mappers under /dev/mapper that are NOT in the pool
/// membership. These are orphans from interrupted add/replace flows --
/// see docs/principles.md:18. An unreadable /dev/mapper is surfaced as
/// Err so callers can warn the user and proceed with an empty orphan
/// set.
fn scan_orphan_mappers<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
) -> Result<Vec<String>, std::io::Error> {
    let entries = fs.list_dir("/dev/mapper")?;
    let mut orphans = Vec::new();
    for entry in entries {
        let Some(disk_name) = name_from_mapper(&entry) else {
            continue;
        };
        if membership.disks.contains_key(disk_name) {
            continue;
        }
        if fs.exists(&format!("/dev/mapper/{entry}")) {
            orphans.push(entry);
        }
    }
    Ok(orphans)
}

/// Message body (no `[warn]` prefix) for a failed /dev/mapper scan.
/// Shared between the dry-run preview and the real-run stderr warn so
/// both branches use identical wording.
fn orphan_scan_warn_body(e: &std::io::Error) -> String {
    format!("could not scan /dev/mapper for orphans: {e} (skipping)")
}

/// Build the scoped `btrfs device scan --forget` argument list: every
/// LUKS mapper path `cmd_lock` is about to destroy (membership +
/// orphan), filtered through fs.exists. The kernel forget path is
/// per-device (reference/linux/fs/btrfs/volumes.c
/// btrfs_free_stale_devices), and `btrfs device scan --forget <path>`
/// rejects non-block-device arguments and aborts on the first failing
/// path, so the list must only contain currently-present mappers.
fn lock_forget_devices<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
    orphan_mappers: &[String],
) -> Vec<String> {
    let mut devs: Vec<String> = membership
        .disks
        .keys()
        .map(|name| format!("/dev/mapper/{}", mapper_name(name).0))
        .filter(|p| fs.exists(p))
        .collect();
    for entry in orphan_mappers {
        let p = format!("/dev/mapper/{entry}");
        if fs.exists(&p) {
            devs.push(p);
        }
    }
    devs
}

/// Compile dry-run steps for lock.
pub fn compile_lock_steps(
    pool_was_mounted: bool,
    open_mappers: &[String],
    orphan_mappers: &[String],
    mount_point: &crate::types::MountPoint,
) -> Vec<Step> {
    let mut steps = Vec::new();

    if pool_was_mounted {
        steps.push(Step {
            risk: "safe",
            description: format!("unmount {}", mount_point),
            commands: vec![CmdRequest::Umount {
                mount_point: mount_point.clone(),
            }],
        });
        let forget_devs: Vec<String> = open_mappers
            .iter()
            .chain(orphan_mappers.iter())
            .map(|m| format!("/dev/mapper/{m}"))
            .collect();
        if !forget_devs.is_empty() {
            steps.push(Step {
                risk: "safe",
                description: "btrfs device scan --forget".into(),
                commands: vec![CmdRequest::BtrfsDeviceScanForget {
                    devices: forget_devs,
                }],
            });
        }
    }

    for mapper in open_mappers {
        steps.push(Step {
            risk: "safe",
            description: format!("close LUKS mapper {}", mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: mapper.clone(),
            }],
        });
    }

    for mapper in orphan_mappers {
        steps.push(Step {
            risk: "safe",
            description: format!("close LUKS mapper {} (orphan)", mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: mapper.clone(),
            }],
        });
    }

    steps
}

/// Render the full `braid lock --dry-run` preview as a single String.
/// The preview includes a `[warn]` line when /dev/mapper cannot be
/// scanned for orphans, followed by either the rendered step block or
/// the literal `nothing to do.` sentinel. This is the user-visible
/// contract boundary for the dry-run output; the caller is a thin
/// printer.
pub fn render_lock_dry_run<F: Filesystem + ?Sized>(
    pool_was_mounted: bool,
    fs: &F,
    membership: &PoolMembership,
    mount_point: &crate::types::MountPoint,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();

    let open_mappers: Vec<String> = membership
        .disks
        .keys()
        .map(|name| mapper_name(name).0)
        .filter(|m| fs.exists(&format!("/dev/mapper/{m}")))
        .collect();

    let orphan_mappers = match scan_orphan_mappers(fs, membership) {
        Ok(v) => v,
        Err(e) => {
            writeln!(out, "[warn]  {}", orphan_scan_warn_body(&e)).unwrap();
            Vec::new()
        }
    };

    let steps = compile_lock_steps(pool_was_mounted, &open_mappers, &orphan_mappers, mount_point);
    if steps.is_empty() {
        out.push_str("nothing to do.\n");
    } else {
        out.push_str(&Step::render_dry_run(&steps));
    }
    out
}

pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError> {
    cmd_lock_impl(runner, fs, &RealSleeper, config, membership, dry_run)
}

fn cmd_lock_impl<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    S: Sleeper,
{
    let mount_point = config.mount_point();

    // 1. Check if pool is mounted
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    let pool_was_mounted = mp_result.exit_status == 0;

    // Preflight
    if pool_was_mounted {
        let fsid = probe_fsid(runner, mount_point)
            .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
        preflight::require_lock_preflight(fs, &fsid).map_err(LockError::Failed)?;
    }

    // Dry-run: render the full preview via render_lock_dry_run and
    // print it to stdout as one stream. The helper owns the orphan scan
    // so a /dev/mapper read failure surfaces as a `[warn]` line inside
    // the preview, mirroring the real-run warn below.
    if dry_run {
        print!(
            "{}",
            render_lock_dry_run(pool_was_mounted, fs, membership, mount_point)
        );
        return Ok(());
    }

    // Runtime orphan probe. Computed once here (after the dry-run
    // early return) so the close loop and the forget device list see
    // the same set. An unreadable /dev/mapper is surfaced as a `[warn]`
    // line (mirroring the dry-run preview) and treated as an empty
    // orphan set -- the primary close loop still runs.
    let orphan_mappers = match scan_orphan_mappers(fs, membership) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[warn]  {}", orphan_scan_warn_body(&e));
            Vec::new()
        }
    };

    // 2. If mounted → unmount
    let mut umount_error: Option<LockError> = None;
    let mut first_mapper_error: Option<LockError> = None;
    if pool_was_mounted {
        let umount_result = runner.run(&CmdRequest::Umount {
            mount_point: mount_point.clone(),
        })?;
        if umount_result.exit_status != 0 {
            let err = LockError::Failed(format!(
                "umount {mount_point} failed (exit {}): {}\n\
                 hint: a process may be using files on the mount. \
                 Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it.",
                umount_result.exit_status,
                umount_result.stderr.trim(),
                mount_point = mount_point,
            ));
            eprintln!("[FAIL]  {err}");
            eprintln!("[warn]  attempting to close LUKS mappers despite umount failure...");
            umount_error = Some(err);
        } else {
            eprintln!("{}  {:<14}unmounted {}", tag("ok"), "pool", mount_point);

            // Clear btrfs kernel scan registry so that cryptsetup close
            // doesn't race against stale device references on multi-device
            // pools. Scope to the close set (membership + orphan mappers)
            // -- the no-arg form is kernel-global and would invalidate
            // scan entries for unrelated btrfs filesystems on the host.
            let forget_devs = lock_forget_devices(fs, membership, &orphan_mappers);
            if !forget_devs.is_empty() {
                let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget {
                    devices: forget_devs,
                });
                match forget_result {
                    Ok(r) if r.exit_status == 0 => {}
                    Ok(r) => {
                        eprintln!(
                            "[warn]  btrfs device scan --forget failed (exit {}): {} (continuing)",
                            r.exit_status,
                            r.stderr.trim()
                        );
                    }
                    Err(e) => {
                        eprintln!("[warn]  btrfs device scan --forget failed: {e} (continuing)");
                    }
                }
            }
        }
    }

    // 3. Close each mapper
    let mut all_already_closed = true;
    for name in membership.disks.keys() {
        let mn = mapper_name(name);
        let mapper_path = format!("/dev/mapper/{}", mn.0);

        if fs.exists(&mapper_path) {
            match close_mapper_with_retry(runner, sleeper, &mn.0) {
                Ok(()) => {
                    eprintln!("{}  disk: {:<7}locked", tag("ok"), name);
                }
                Err(LockError::DeviceBusy(msg)) if umount_error.is_some() => {
                    eprintln!(
                        "[warn]  disk: {:<7}close failed (umount was stuck): {}",
                        name, msg
                    );
                }
                Err(e) => {
                    eprintln!("[FAIL]  disk: {:<7}{}", name, e);
                    if first_mapper_error.is_none() {
                        first_mapper_error = Some(e);
                    }
                }
            }
            all_already_closed = false;
        } else {
            eprintln!("{}  disk: {:<7}already closed", tag("ok"), name);
        }
    }

    // 3b. Close orphaned braid-* mappers (precomputed above so the
    // forget call shared the same close-set). An orphan is detected
    // iff fs.exists was true at probe time; re-check to cover the
    // narrow window where it disappeared on its own.
    for entry in &orphan_mappers {
        let disk_name = name_from_mapper(entry).unwrap_or(entry);
        if !fs.exists(&format!("/dev/mapper/{entry}")) {
            continue;
        }
        eprintln!(
            "[warn]  orphaned mapper {entry} (not in pool.json -- likely a prior crash)"
        );
        match close_mapper_with_retry(runner, sleeper, entry) {
            Ok(()) => {
                eprintln!("{}  disk: {:<7}locked (orphan)", tag("ok"), disk_name);
            }
            Err(LockError::DeviceBusy(msg)) if umount_error.is_some() => {
                eprintln!(
                    "[warn]  disk: {:<7}orphan close failed (umount was stuck): {}",
                    disk_name, msg
                );
            }
            Err(e) => {
                eprintln!("[FAIL]  disk: {:<7}orphan: {}", disk_name, e);
                if first_mapper_error.is_none() {
                    first_mapper_error = Some(e);
                }
            }
        }
        all_already_closed = false;
    }

    // 4. Return first fatal mapper error if any, otherwise deferred umount error
    if let Some(e) = first_mapper_error {
        return Err(e);
    }
    if let Some(e) = umount_error {
        return Err(e);
    }

    // 5. If nothing was done → short message
    if !pool_was_mounted && all_already_closed {
        eprintln!("pool already locked");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    /// A runner that delegates to MockRunner but records which
    /// CryptsetupClose requests were made. Optionally serves a per-mapper
    /// queue of CryptsetupClose responses (drained in order) before
    /// falling back to the inner mock -- used to model transient busy
    /// errors that succeed on retry.
    struct RecordingRunner {
        inner: MockRunner,
        close_calls: Mutex<Vec<String>>,
        close_sequences: Mutex<HashMap<String, VecDeque<RawCommandOutput>>>,
        forget_calls: Mutex<Vec<Vec<String>>>,
    }

    impl RecordingRunner {
        fn new(inner: MockRunner) -> Self {
            Self {
                inner,
                close_calls: Mutex::new(Vec::new()),
                close_sequences: Mutex::new(HashMap::new()),
                forget_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_close_sequence(self, mapper: &str, outputs: Vec<RawCommandOutput>) -> Self {
            self.close_sequences
                .lock()
                .unwrap()
                .insert(mapper.to_owned(), outputs.into());
            self
        }

        fn close_calls(&self) -> Vec<String> {
            self.close_calls.lock().unwrap().clone()
        }

        fn forget_calls(&self) -> Vec<Vec<String>> {
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

    struct MockFs {
        paths: Vec<String>,
        exclop: String,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                exclop: "none\n".to_owned(),
            }
        }

        fn with_exclop(mut self, exclop: &str) -> Self {
            self.exclop = format!("{exclop}\n");
            self
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok(self.exclop.clone())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }

        fn list_dir(&self, dir: &str) -> Result<Vec<String>, std::io::Error> {
            let prefix = if dir.ends_with('/') {
                dir.to_string()
            } else {
                format!("{dir}/")
            };
            let entries: Vec<String> = self
                .paths
                .iter()
                .filter_map(|p| p.strip_prefix(&prefix).map(|s| s.to_string()))
                .filter(|s| !s.contains('/'))
                .collect();
            Ok(entries)
        }
    }

    /// Test sleeper that records zero wall time. Default choice for
    /// every lock test that drives `cmd_lock_impl`; the retry loop still
    /// executes the correct number of iterations, just without blocking.
    struct NoopSleeper;
    impl Sleeper for NoopSleeper {
        fn sleep(&self, _duration: Duration) {}
    }

    fn ok_raw(cmd: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    /// Add the minimal `probe_fsid` preflight mocks to a runner
    /// (FindmntJson + BtrfsFilesystemShow). Used by tests that build
    /// their mock runner from scratch and still need to land in the
    /// "mounted, btrfs, fsid=aaaa..." preflight path.
    fn with_fsid_probe_mocks(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "findmnt --json".into(),
                    stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-aaa","fstype":"btrfs","options":"rw"}]}"#.into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
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
            )
    }

    fn test_config() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn test_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        disks.insert(
            "aaa".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/a".to_owned())),
        );
        disks.insert(
            "bbb".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/b".to_owned())),
        );
        PoolMembership { disks }
    }

    /// Build a MockRunner pre-loaded with the minimal preflight outputs
    /// cmd_lock actually issues (mountpoint check = mounted,
    /// probe_fsid mocks, umount = ok, forget = ok).
    ///
    /// Only FindmntJson + BtrfsFilesystemShow are registered for the
    /// preflight path. Per-device CryptsetupStatus / CryptsetupLuksUuid
    /// are intentionally absent: MockRunner panics on any unregistered
    /// CmdRequest, so lock tests passing without those mocks is the
    /// mechanical regression guard that cmd_lock no longer issues them.
    fn mounted_runner() -> MockRunner {
        with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("umount /mnt/storage"),
        )
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![
                    "/dev/mapper/braid-aaa".into(),
                    "/dev/mapper/braid-bbb".into(),
                ],
            },
            ok_raw("btrfs device scan --forget"),
        )
    }

    #[test]
    fn lock_happy_path_unmounts_and_closes() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect("lock should succeed");
    }

    #[test]
    fn lock_already_locked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = MockFs::new(&[]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed (already locked)");
    }

    #[test]
    fn lock_partial_state() {
        // Pool not mounted, only aaa mapper open
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint -q /mnt/storage", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect("lock should succeed (partial)");
    }

    // Intent: lock fails when umount reports the mount is busy.
    // Why it exists: a busy mount means the pool cannot be cleanly locked;
    //   reporting success would be a lie.
    // Scenario: a process holds a file open on /mnt/storage; umount returns
    //   "target is busy". lock still attempts mapper close (best-effort), but
    //   ultimately returns the umount error.
    #[test]
    fn lock_umount_busy_fails() {
        let runner = with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("umount /mnt/storage", 32, "target is busy"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err =
            cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
    }

    // Intent: the umount-busy error message includes actionable diagnostic hints.
    // Why it exists: users need to know how to find the blocking process so
    //   they can kill it and retry lock.
    // Scenario: umount fails with "target is busy"; the error message suggests
    //   running lsof or fuser to identify the holder.
    #[test]
    fn lock_umount_busy_includes_hint() {
        let runner = with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("umount /mnt/storage", 32, "target is busy"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err =
            cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect_err("should fail on busy");
        let msg = err.to_string();
        assert!(
            msg.contains("lsof") && msg.contains("fuser"),
            "expected lsof/fuser hint in error, got: {msg}"
        );
    }

    #[test]
    fn lock_adds_forget_after_umount() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        // If BtrfsDeviceScanForget were not called, MockRunner would return
        // MissingMock and the test would fail.
        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed with forget");
    }

    #[test]
    fn lock_forget_failure_is_nonfatal() {
        let runner = with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("umount /mnt/storage"),
        )
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![
                    "/dev/mapper/braid-aaa".into(),
                    "/dev/mapper/braid-bbb".into(),
                ],
            },
            err_raw("btrfs device scan --forget", 1, "some error"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed even when forget fails");
    }

    // Intent: orphaned braid-* mappers from prior crashes are cleaned up
    //   during lock.
    // Why it exists: a crash between cryptsetup open and journal/pool.json
    //   write leaves a mapper outside pool.json that the membership loop
    //   won't close.
    // Scenario: power loss during `braid add` after LUKS open but before
    //   pool.json write; next `braid lock` must still close the orphan.
    #[test]
    fn lock_closes_orphaned_mapper() {
        let runner = mounted_runner()
            // Override forget mock: with an orphan present, the forget
            // set must include it (close-set-scoped).
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-ccc".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                ok_raw("cryptsetup close braid-ccc"),
            );
        // ccc is not in membership but exists as a mapper → orphan
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect("lock should close orphan too");
    }

    // Intent: I/O errors scanning /dev/mapper don't prevent closing known
    //   mappers.
    // Why it exists: /dev/mapper may be unreadable in degraded environments;
    //   the safety-net scan shouldn't break the primary lock path.
    // Scenario: containerized environment where /dev/mapper has restricted
    //   permissions; lock must still close membership-known mappers.
    #[test]
    fn lock_orphan_scan_failure_is_nonfatal() {
        struct FailListDirFs;
        impl Filesystem for FailListDirFs {
            fn exists(&self, path: &str) -> bool {
                path == "/dev/mapper/braid-aaa" || path == "/dev/mapper/braid-bbb"
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
            fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
                if path.ends_with("/exclusive_operation") {
                    Ok("none\n".to_owned())
                } else {
                    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
                }
            }
            fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
            }
        }

        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &FailListDirFs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed despite list_dir failure");
    }

    /*
     * Intent: `braid lock --dry-run` preview surfaces a `[warn]` line when
     *   /dev/mapper cannot be scanned for orphans.
     * Why it exists: the dry-run branch previously used
     *   `if let Ok(entries) = fs.list_dir(...)`, silently swallowing the
     *   error while the real run warned -- violating the dry-run contract
     *   of "preview what the real command will do."
     * Scenario: containerized environment where /dev/mapper is unreadable;
     *   the user runs `braid lock --dry-run` to preview the shutdown and
     *   must see the scan failure, not a falsely-clean preview.
     */
    #[test]
    fn dry_run_preview_warns_when_list_dir_fails() {
        struct FailListDirFs;
        impl Filesystem for FailListDirFs {
            fn exists(&self, path: &str) -> bool {
                path == "/dev/mapper/braid-aaa" || path == "/dev/mapper/braid-bbb"
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
            fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
            fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
            }
        }

        let mount_point = MountPoint("/mnt/storage".to_owned());
        let output = render_lock_dry_run(false, &FailListDirFs, &test_membership(), &mount_point);

        assert!(
            output.starts_with(
                "[warn]  could not scan /dev/mapper for orphans: permission denied (skipping)\n"
            ),
            "preview must start with the exact [warn] line, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-aaa"),
            "preview must still render membership close steps, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-bbb"),
            "preview must still render membership close steps, got:\n{output}"
        );
    }

    /*
     * Intent: `render_lock_dry_run` renders the full happy-path preview --
     *   umount + scoped forget + per-mapper closes (membership and orphan).
     * Why it exists: the preview helper is the sole boundary between
     *   `cmd_lock` dry-run and the user; a refactor that drops any of
     *   these steps must fail a test, not only `compile_lock_steps`'
     *   isolated tests.
     * Scenario: pool mounted, both membership mappers open, one orphan
     *   (braid-ccc) left by a prior crash; user previews `braid lock
     *   --dry-run` to confirm the shutdown plan before running it.
     */
    #[test]
    fn dry_run_preview_mounted_happy_path() {
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let mount_point = MountPoint("/mnt/storage".to_owned());
        let output = render_lock_dry_run(true, &fs, &test_membership(), &mount_point);

        assert!(
            !output.contains("[warn]"),
            "happy path must not emit a scan warning, got:\n{output}"
        );
        assert!(
            output.contains("unmount /mnt/storage"),
            "preview must include unmount step, got:\n{output}"
        );
        assert!(
            output.contains("btrfs device scan --forget"),
            "preview must include scoped forget step, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-aaa"),
            "preview must include membership close for braid-aaa, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-bbb"),
            "preview must include membership close for braid-bbb, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-ccc (orphan)"),
            "preview must include orphan close for braid-ccc, got:\n{output}"
        );
    }

    /*
     * Intent: `render_lock_dry_run` emits exactly `nothing to do.\n` when
     *   the pool is unmounted with no open membership or orphan mappers.
     * Why it exists: the no-op branch is easy to regress silently -- a
     *   helper refactor could drop or alter the line and all other tests
     *   would stay green.
     * Scenario: user re-runs `braid lock --dry-run` on an already-locked
     *   pool and expects a short, deterministic confirmation.
     */
    #[test]
    fn dry_run_preview_nothing_to_do() {
        let fs = MockFs::new(&[]);
        let mount_point = MountPoint("/mnt/storage".to_owned());
        let output = render_lock_dry_run(false, &fs, &test_membership(), &mount_point);

        assert_eq!(output, "nothing to do.\n", "unexpected preview: {output:?}");
    }

    /// Build a MockRunner pre-loaded with a failed-umount scenario
    /// (mountpoint check = mounted, balance status = no balance, umount = busy).
    /// No BtrfsDeviceScanForget — forget is gated on successful unmount.
    fn umount_failed_runner() -> MockRunner {
        with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("umount /mnt/storage", 32, "target is busy"),
        )
    }

    // Intent: when umount fails, lock still attempts to close LUKS mappers
    //   and returns the umount error (not a mapper error).
    // Why it exists: the original code returned immediately on umount failure,
    //   leaving all LUKS mappers open — a security gap during shutdown.
    // Scenario: umount fails with "target is busy"; both mapper closes succeed
    //   anyway (e.g. kernel released references between umount and close).
    //   The function still fails with the umount error because the mount is
    //   in an inconsistent state.
    #[test]
    fn lock_umount_fails_but_mappers_close_successfully() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail — umount error is the root cause");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error, got: {msg}"
        );
    }

    // Intent: busy mapper close errors are suppressed (as warnings) when
    //   umount already failed, and the umount error is returned.
    // Why it exists: busy mapper close after a stuck umount is expected —
    //   the filesystem still holds the devices. Surfacing the mapper error
    //   instead of the umount error would obscure the root cause.
    // Scenario: umount fails; both mapper closes fail with "in use" (DeviceBusy).
    //   The returned error is the umount error, not a mapper close error.
    #[test]
    fn lock_umount_fails_busy_mapper_is_warning() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not mapper error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) mapper close errors remain fatal even when
    //   umount already failed — only DeviceBusy is suppressed.
    // Why it exists: suppressing all mapper close errors after umount failure
    //   would hide real problems like permission errors or missing devices.
    //   Only the expected busy/in-use errors should be downgraded to warnings.
    // Scenario: umount fails; mapper aaa close fails with "Device is not
    //   active." (not a busy error). Remaining mappers are still attempted,
    //   then the non-busy mapper error is returned (takes precedence over
    //   the umount error).
    #[test]
    fn lock_umount_fails_unexpected_mapper_error_is_fatal() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with mapper error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa") && msg.contains("not active"),
            "expected mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: mapper close errors remain fatal when umount succeeded.
    // Why it exists: regression guard — the umount-failure fix must not
    //   accidentally suppress mapper close errors on the normal path.
    // Scenario: umount succeeds; aaa mapper close fails with a non-busy error.
    //   Remaining mappers are still attempted, then the mapper error is returned.
    #[test]
    fn lock_mapper_close_fatal_when_umount_succeeded() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail on mapper close");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa"),
            "expected mapper error, got: {msg}"
        );
    }

    // Intent: busy orphan mapper close errors are suppressed when umount
    //   already failed, same as for membership mappers.
    // Why it exists: the membership and orphan close loops are separate code
    //   paths; a bug in orphan handling could slip through even if the
    //   membership tests pass.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "in use" (DeviceBusy). The returned error is the
    //   umount error.
    #[test]
    fn lock_umount_fails_orphan_busy_is_warning() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                err_raw(
                    "cryptsetup close braid-ccc",
                    5,
                    "Device braid-ccc is still in use.",
                ),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not orphan error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) orphan mapper close errors remain fatal
    //   even when umount already failed.
    // Why it exists: the orphan branch must have the same precise suppression
    //   as the membership branch — only DeviceBusy is suppressed.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "Device is not active." (non-busy). All mappers are
    //   still attempted, then the orphan error is returned (takes precedence
    //   over the umount error).
    #[test]
    fn lock_umount_fails_orphan_unexpected_error_is_fatal() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                err_raw("cryptsetup close braid-ccc", 4, "Device is not active."),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with orphan error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-ccc") && msg.contains("not active"),
            "expected orphan mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: if an orphan mapper is detected but can't be closed, lock must
    //   fail rather than silently leaving LUKS open.
    // Why it exists: a stray open LUKS mapper is a security concern —
    //   reporting success while leaving it open is worse than failing.
    // Scenario: orphan mapper is held open by a leaked process; lock must
    //   surface the failure.
    #[test]
    fn lock_orphan_close_failure_is_fatal() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-orphan".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-orphan".into(),
                },
                err_raw("cryptsetup close braid-orphan", 4, "Device is not active."),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-orphan",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail on orphan close");
        assert!(
            err.to_string().contains("braid-orphan"),
            "error should mention the orphan mapper, got: {err}"
        );
    }

    // Intent: when a mapper close fails with a non-busy error, remaining
    //   mappers are still attempted.
    // Why it exists: guards against the original bug where a non-busy error
    //   caused an early return, skipping remaining mappers and leaving LUKS
    //   devices open.
    // Scenario: umount succeeds; aaa mapper close fails with "Device is not
    //   active"; bbb mapper close succeeds. Both mappers were attempted.
    #[test]
    fn lock_continues_closing_after_mapper_error() {
        let inner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = RecordingRunner::new(inner);
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with mapper error");
        assert!(
            err.to_string().contains("braid-aaa"),
            "expected aaa error, got: {err}"
        );
        let calls = runner.close_calls();
        assert!(
            calls.contains(&"braid-aaa".to_string()) && calls.contains(&"braid-bbb".to_string()),
            "expected both mappers attempted, got: {calls:?}"
        );
    }

    // Intent: when multiple mapper closes fail with non-busy errors, the
    //   first error is returned and all mappers were attempted.
    // Why it exists: ensures error accumulation works end-to-end for the
    //   multi-failure case — the first error wins, but nothing is skipped.
    // Scenario: umount succeeds; both aaa and bbb fail with non-busy errors.
    //   The returned error mentions aaa (first in iteration order).
    #[test]
    fn lock_collects_first_mapper_error() {
        let inner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw("cryptsetup close braid-bbb", 1, "permission denied"),
            );
        let runner = RecordingRunner::new(inner);
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail with first mapper error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa"),
            "expected first error (aaa), got: {msg}"
        );
        let calls = runner.close_calls();
        assert!(
            calls.contains(&"braid-aaa".to_string()) && calls.contains(&"braid-bbb".to_string()),
            "expected both mappers attempted, got: {calls:?}"
        );
    }

    /*
     * Intent: `cryptsetup close` that returns "busy" once but succeeds on
     * retry must let `braid lock` finish cleanly, closing the mapper on
     * attempt 2.
     *
     * Why it exists: the btrfs scan registry can keep device references
     * alive for a short window after umount (see commit 1484ff1 and
     * tests/repro/cryptsetup-close-btrfs-held.py). The retry loop in
     * `close_mapper_with_retry` exists to cover that window. Without
     * this test, a regression that misclassifies the busy substring,
     * flips CLOSE_RETRY_ATTEMPTS to 1, or mis-orders the early returns
     * would pass every existing unit test -- only the race-dependent VM
     * repro could surface it.
     *
     * Scenario: pool mounted; umount and btrfs forget succeed; first
     * `cryptsetup close braid-aaa` returns "Device braid-aaa is still
     * in use.", second returns ok; `braid-bbb` closes cleanly on the
     * first try.
     */
    #[test]
    fn lock_retries_busy_close_then_succeeds() {
        let inner = mounted_runner().with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            ok_raw("cryptsetup close braid-bbb"),
        );
        let runner = RecordingRunner::new(inner).with_close_sequence(
            "braid-aaa",
            vec![
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
                ok_raw("cryptsetup close braid-aaa"),
            ],
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed after retry");

        let calls = runner.close_calls();
        let aaa_calls = calls.iter().filter(|m| m.as_str() == "braid-aaa").count();
        let bbb_calls = calls.iter().filter(|m| m.as_str() == "braid-bbb").count();
        assert_eq!(
            aaa_calls, 2,
            "expected exactly 2 close attempts for braid-aaa, got: {calls:?}"
        );
        assert_eq!(
            bbb_calls, 1,
            "expected exactly 1 close for braid-bbb, got: {calls:?}"
        );
    }

    // Intent: cryptsetup close with exit status 5 goes through the retry
    //   loop and surfaces as LockError::DeviceBusy, regardless of the
    //   specific English phrase in stderr.
    // Why it exists: the classifier at lock.rs:38-42 is what distinguishes
    //   "kernel-async release race, retry wins" from "every close
    //   hard-fails on first attempt". An earlier stderr-substring
    //   classifier ("busy" || "in use") would hard-fail on wording drift
    //   like "still active and cannot be removed". This test uses that
    //   non-canonical wording at exit 5 so a regression back to
    //   stderr-based matching fails here.
    // Scenario: umount succeeds; braid-aaa close returns exit 5 on every
    //   attempt with non-canonical busy wording; braid-bbb closes cleanly.
    //   Lock must retry braid-aaa CLOSE_RETRY_ATTEMPTS times, then return
    //   LockError::DeviceBusy.
    #[test]
    fn lock_mapper_close_exit5_is_busy_regardless_of_wording() {
        let inner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Target braid-aaa is still active and cannot be removed.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = RecordingRunner::new(inner);
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("busy close should bubble up after retries exhaust");
        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected LockError::DeviceBusy, got: {err:?}"
        );
        let aaa_attempts = runner
            .close_calls()
            .iter()
            .filter(|m| m.as_str() == "braid-aaa")
            .count();
        assert_eq!(
            aaa_attempts, CLOSE_RETRY_ATTEMPTS as usize,
            "expected {} retry attempts, got {}",
            CLOSE_RETRY_ATTEMPTS, aaa_attempts
        );
    }

    // Intent: when `cryptsetup close` returns busy on every retry and umount
    //   succeeded (no suppression), cmd_lock surfaces a LockError::DeviceBusy
    //   whose rendered message preserves the mapper name, the raw exit code,
    //   and the ORIGINAL-CASED, TRIMMED stderr from cryptsetup exactly.
    // Why it exists: locks the full DeviceBusy message contract so refactors
    //   in close_mapper_with_retry (dedup, formatting tweaks) can't silently
    //   drop .trim(), change the shape, or drift the text. The sibling test
    //   `lock_mapper_close_exit5_is_busy_regardless_of_wording` pins variant
    //   + retry count but not message content; this test pins the exact
    //   bytes the user sees.
    // Scenario: pool mounted; umount/forget succeed; every close attempt
    //   for braid-aaa returns exit 5 with a mixed-case stderr padded with
    //   leading whitespace and a trailing newline; braid-bbb closes cleanly.
    #[test]
    fn lock_busy_close_exhausts_retries_preserves_stderr_contract() {
        let busy_stderr = "  Device braid-aaa IS STILL IN USE.\n";
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw("cryptsetup close braid-aaa", 5, busy_stderr),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should fail: busy retries exhausted");
        // Variant check is orthogonal to text check: guards against a rename
        // that also updates the #[error(...)] attribute and still renders
        // the same bytes.
        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected LockError::DeviceBusy, got: {err:?}"
        );
        // Full rendered-message lock: pins the thiserror prefix
        // ("device busy: "), the cryptsetup phrasing, the raw exit code,
        // and the ORIGINAL-CASED TRIMMED stderr all in one assertion. Any
        // drift -- shape change, dropped .trim(), missing exit code --
        // flips this.
        assert_eq!(
            err.to_string(),
            "device busy: cryptsetup close braid-aaa failed (exit 5): \
             Device braid-aaa IS STILL IN USE."
        );
    }

    #[test]
    // Intent: lock refuses when any exclusive op is active (running balance).
    // Why: unmounting during an exclusive op is unsafe — data corruption risk.
    // Scenario: a RAID1 balance is in progress, operator runs `braid lock`.
    //   Lock must refuse without unmounting or closing any mappers.
    fn lock_refuses_when_exclusive_op_active() {
        let runner = with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs =
            MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]).with_exclop("balance");
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should refuse — balance is active");
        let msg = err.to_string();
        assert!(
            msg.contains("balance") && msg.contains("in progress"),
            "expected active-op refusal, got: {msg}"
        );
    }

    #[test]
    // Intent: lock refuses when a balance is paused.
    // Why: a paused balance still holds the exclusive lock — unmounting is unsafe.
    // Scenario: operator paused a balance and forgot, then runs `braid lock`.
    fn lock_refuses_when_balance_paused() {
        let runner = with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_exclop("balance paused");
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should refuse — balance is paused");
        let msg = err.to_string();
        assert!(
            msg.contains("in progress"),
            "expected paused-balance refusal, got: {msg}"
        );
    }

    // Intent: cmd_lock preserves probe_pool's NotBtrfs contract through
    //   probe_fsid -- if the mount point is held by a non-btrfs
    //   filesystem, lock fails with a typed message naming the fstype
    //   rather than a generic btrfs-show parse failure.
    // Why: the refactor from probe_pool to probe_fsid dropped per-device
    //   cryptsetup checks; it must NOT also drop the mounted-non-btrfs
    //   check. Without this guard, an ext4-mounted /mnt/storage would
    //   fall through to `btrfs filesystem show`, fail with a confusing
    //   parse error, and mask the real mount-configuration issue.
    // Scenario: MountpointCheck succeeds, findmnt reports the mount
    //   point's fstype as ext4. cmd_lock must refuse with a
    //   LockError::Failed whose message mentions "not btrfs".
    #[test]
    fn lock_rejects_mounted_but_not_btrfs() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "findmnt --json".into(),
                    stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/sda1","fstype":"ext4","options":"rw"}]}"#.into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect_err("should refuse -- mount is not btrfs");
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "expected NotBtrfs-style message naming ext4, got: {msg}"
        );
    }

    // Intent: dry-run for lock shows umount + scan forget + close per open mapper.
    // Why: verifies compile_lock_steps produces correct output. The
    // rendered forget command must include the explicit device paths,
    // not the bare kernel-global form.
    // Scenario: pool mounted, 2 open mappers, no orphans.
    #[test]
    fn dry_run_render_lock_mounted_2_disks() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let open_mappers = vec!["braid-disk1".into(), "braid-disk2".into()];
        let steps = compile_lock_steps(true, &open_mappers, &[], &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 4 steps (umount + scan forget + 2× close), each with 1 command = 8 lines
        assert_eq!(lines.len(), 8, "expected 8 lines, got:\n{output}");
        assert!(lines[0].contains("unmount"));
        assert!(lines[1].contains("$ umount"));
        assert!(lines[2].contains("btrfs device scan --forget"));
        assert!(
            lines[3].contains("--forget /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"),
            "rendered forget command must list pool mapper paths, got: {}",
            lines[3]
        );
        assert!(lines[4].contains("close LUKS mapper braid-disk1"));
        assert!(lines[6].contains("close LUKS mapper braid-disk2"));
    }

    // Intent: dry-run when not mounted skips umount/scan, shows only close.
    // Why: verifies conditional step omission.
    // Scenario: pool not mounted, 1 mapper still open.
    #[test]
    fn dry_run_lock_not_mounted_1_open() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let open_mappers = vec!["braid-disk1".into()];
        let steps = compile_lock_steps(false, &open_mappers, &[], &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 1 step (close), 2 lines
        assert_eq!(lines.len(), 2, "expected 2 lines, got:\n{output}");
        assert!(lines[0].contains("close LUKS mapper"));
        assert!(!output.contains("unmount"));
    }

    // Intent: dry-run when nothing to do returns empty steps.
    // Why: verifies the "nothing to do" case.
    // Scenario: pool not mounted, all mappers closed, no orphans.
    #[test]
    fn dry_run_lock_nothing_to_do() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_lock_steps(false, &[], &[], &mount_point);
        assert!(steps.is_empty());
    }

    /// Extract the `devices` list from the single forget step in a
    /// compiled lock plan. Panics if there is not exactly one forget
    /// step, so the caller's assertion is anchored to a present step.
    fn forget_step_devices(steps: &[Step]) -> Vec<String> {
        let mut found: Option<Vec<String>> = None;
        for step in steps {
            for cmd in &step.commands {
                if let CmdRequest::BtrfsDeviceScanForget { devices } = cmd {
                    assert!(
                        found.is_none(),
                        "multiple forget steps in plan: {steps:?}"
                    );
                    found = Some(devices.clone());
                }
            }
        }
        found.expect("no forget step in plan")
    }

    fn count_forget_steps(steps: &[Step]) -> usize {
        steps
            .iter()
            .flat_map(|s| &s.commands)
            .filter(|c| matches!(c, CmdRequest::BtrfsDeviceScanForget { .. }))
            .count()
    }

    // Intent: the compiled dry-run plan's forget step lists the pool's
    // own mapper paths, never the kernel-global no-arg form.
    // Why: the no-arg form (btrfs_forget_devices(NULL) in
    // reference/btrfs-progs/cmds/device.c) invalidates every btrfs scan
    // entry on the host. Pool-scoping matters as soon as a second
    // (non-braid) btrfs filesystem coexists.
    // Scenario: 2-disk pool, no orphans; the forget step must carry
    // exactly the pool's mapper paths in membership order.
    #[test]
    fn dry_run_lock_forget_step_lists_scoped_devices() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let open_mappers = vec!["braid-aaa".into(), "braid-bbb".into()];
        let steps = compile_lock_steps(true, &open_mappers, &[], &mount_point);
        assert_eq!(
            forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ],
        );
    }

    // Intent: dry-run's forget step unions membership + orphan mappers
    // -- the exact set compile_lock_steps will also close below it.
    // Why: the kernel forget path is per-device, not per-fsid
    // (reference/linux/fs/btrfs/volumes.c btrfs_free_stale_devices).
    // Forgetting only membership leaves an orphan mapper (from a prior
    // crash between cryptsetup open and pool.json write, per
    // docs/principles.md:18) with a stale scan entry, reviving the
    // cryptsetup-close-btrfs-held race for the orphan.
    // Scenario: 1 membership mapper, 1 orphan; forget devices = union.
    #[test]
    fn dry_run_lock_forget_step_includes_orphans() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let open_mappers = vec!["braid-aaa".into()];
        let orphan_mappers = vec!["braid-orphan".into()];
        let steps = compile_lock_steps(true, &open_mappers, &orphan_mappers, &mount_point);
        assert_eq!(
            forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-orphan".to_string(),
            ],
        );
    }

    // Intent: the forget step is omitted entirely when there are no
    // mappers to close, even if the pool was mounted.
    // Why: a forget call with no arguments is kernel-global. The only
    // safe way to express "forget nothing" is to not issue the command
    // at all.
    // Scenario: pool_was_mounted=true but membership and orphan lists
    // are both empty -- only the umount step remains in the plan.
    #[test]
    fn dry_run_lock_forget_step_omitted_when_no_mappers() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_lock_steps(true, &[], &[], &mount_point);
        assert_eq!(count_forget_steps(&steps), 0, "no forget step expected");
        assert!(
            steps.iter().any(|s| s
                .commands
                .iter()
                .any(|c| matches!(c, CmdRequest::Umount { .. }))),
            "umount step should still be emitted",
        );
    }

    // Intent: `braid lock` scopes the forget request to the pool's own
    // mappers, never the kernel-global no-arg form.
    // Why: the no-arg form invalidates every btrfs scan entry on the
    // host (reference/btrfs-progs/cmds/device.c:btrfs_forget_devices
    // with path=NULL). Pool-scoping prevents `braid lock` from
    // clobbering scan state for an unrelated btrfs filesystem.
    // Scenario: 2-disk pool, no orphans; the recorded forget call
    // carries exactly the pool's mapper paths.
    #[test]
    fn lock_forget_is_pool_scoped() {
        let inner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = RecordingRunner::new(inner);
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false).expect("lock should succeed");

        assert_eq!(
            runner.forget_calls(),
            vec![vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ]],
            "forget must be pool-scoped (not kernel-global, not membership-only)"
        );
    }

    // Intent: `braid lock` forgets the full close set -- membership AND
    // orphan mappers.
    // Why: the kernel forget path is per-device
    // (reference/linux/fs/btrfs/volumes.c btrfs_free_stale_devices).
    // Membership-only forget leaves crash-created orphan mappers with
    // stale scan entries, reviving the cryptsetup-close-btrfs-held
    // race that BtrfsDeviceScanForget exists to prevent (see
    // tests/repro/cryptsetup-close-btrfs-held.py).
    // Scenario: 2-disk pool + 1 orphan (braid-ccc); the recorded forget
    // call carries all three mapper paths.
    #[test]
    fn lock_forget_includes_orphan_mappers() {
        let inner = mounted_runner()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-ccc".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                ok_raw("cryptsetup close braid-ccc"),
            );
        let runner = RecordingRunner::new(inner);
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock_impl(&runner, &fs, &NoopSleeper, &config, &membership, false)
            .expect("lock should succeed and close orphan");

        assert_eq!(
            runner.forget_calls(),
            vec![vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
                "/dev/mapper/braid-ccc".to_string(),
            ]],
            "forget must include the orphan mapper in the close set"
        );
    }

    /*
     * Intent: close_mapper_with_retry sleeps exactly CLOSE_RETRY_DELAY
     *   between busy attempts, and the prod value of CLOSE_RETRY_DELAY
     *   remains 500ms.
     *
     * Why it exists: the retry delay papers over a kernel-level race
     *   between umount and cryptsetup close on multi-device btrfs (see
     *   commit 1484ff1 and tests/repro/cryptsetup-close-btrfs-held.py).
     *   The repro test is race-dependent and the CLI-level VM test
     *   braid-lock-btrfs-held.py relies on the same race to trigger the
     *   retry path -- neither deterministically catches a regression
     *   that removes, zeroes, or bypasses the sleep. This test locks
     *   the contract at the helper.
     *
     * Scenario: a busy close error repeats for all CLOSE_RETRY_ATTEMPTS
     *   tries; the RecordingSleeper captures (CLOSE_RETRY_ATTEMPTS - 1)
     *   sleep calls, each exactly CLOSE_RETRY_DELAY, and the returned
     *   error is DeviceBusy.
     */
    #[test]
    fn close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts() {
        struct RecordingSleeper(Mutex<Vec<Duration>>);
        impl Sleeper for RecordingSleeper {
            fn sleep(&self, d: Duration) {
                self.0.lock().unwrap().push(d);
            }
        }

        let sleeper = RecordingSleeper(Mutex::new(Vec::new()));
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        );

        let err = close_mapper_with_retry(&runner, &sleeper, "braid-aaa")
            .expect_err("should exhaust retries and return DeviceBusy");
        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected DeviceBusy after retry exhaustion, got: {err:?}"
        );

        let recorded = sleeper.0.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            (CLOSE_RETRY_ATTEMPTS - 1) as usize,
            "expected one sleep between each pair of attempts, got: {recorded:?}"
        );
        for d in &recorded {
            assert_eq!(
                *d, CLOSE_RETRY_DELAY,
                "each retry must sleep CLOSE_RETRY_DELAY, got: {recorded:?}"
            );
        }
        assert_eq!(
            CLOSE_RETRY_DELAY,
            Duration::from_millis(500),
            "prod CLOSE_RETRY_DELAY must stay 500ms; if you intend to \
             change this, update the kernel-race justification in the \
             commit message"
        );
    }

    /*
     * Intent: public cmd_lock wires a real sleeper. An always-busy
     *   mapper makes the wrapper pay measurable wall-clock sleep time
     *   before returning DeviceBusy, proving &RealSleeper (not
     *   &NoopSleeper) is on the hot path.
     *
     * Why it exists: the helper-level RecordingSleeper test proves
     *   close_mapper_with_retry uses CLOSE_RETRY_DELAY, but does not
     *   prove the public wrapper hands in &RealSleeper. A regression
     *   that shipped &NoopSleeper (or dropped the sleeper entirely) in
     *   production would leave lock reliability race-dependent and
     *   pass every helper-level unit test -- including
     *   braid-lock-btrfs-held.py, which only asserts success and does
     *   not deterministically force the retry path.
     *
     * Scenario: umount succeeds, then every mapper close returns
     *   "is still in use" so the retry loop runs to exhaustion. Because
     *   umount did not set umount_error, DeviceBusy is NOT suppressed:
     *   it becomes first_mapper_error and is the returned value. Wall
     *   time is bounded below by (CLOSE_RETRY_ATTEMPTS - 1) *
     *   CLOSE_RETRY_DELAY for a single mapper; we assert a tolerant
     *   lower bound of that amount to stay robust on slow CI while
     *   still failing loudly if no real sleep happened.
     */
    #[test]
    fn cmd_lock_wrapper_uses_real_sleeper() {
        use std::time::Instant;

        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let start = Instant::now();
        let err = cmd_lock(&runner, &fs, &config, &membership, false)
            .expect_err("should fail with DeviceBusy after retry exhaustion");
        let elapsed = start.elapsed();

        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected DeviceBusy from public wrapper, got: {err:?}"
        );

        // Both mappers hit the full retry loop: expected total real
        // sleep is 2 * (CLOSE_RETRY_ATTEMPTS - 1) * CLOSE_RETRY_DELAY =
        // 2s. We assert a tolerant lower bound of one mapper's worth
        // (~900ms) so scheduler jitter on slow CI does not cause flake,
        // while still catching a NoopSleeper regression (which would
        // complete in microseconds).
        let min_expected =
            CLOSE_RETRY_DELAY * (CLOSE_RETRY_ATTEMPTS - 1) - Duration::from_millis(100);
        assert!(
            elapsed >= min_expected,
            "wrapper must use RealSleeper -- elapsed {:?} < min {:?}",
            elapsed,
            min_expected,
        );
    }
}
