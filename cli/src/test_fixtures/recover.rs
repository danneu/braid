//! Recover-scope fixtures: `RecoverParamsBuilder` over `RecoverParams`,
//! `RemountHarness` (the promoted stateful-FS + mapper-closing-runner
//! pair), and the `recover_params(&self)` builder seed on
//! `PoolFixture`.
//!
//! Recover replays committed mutations from any of
//! add/remove/remove-missing/replace journals plus drives its own probe
//! surface plus exercises the remount cycle. A single broad topology
//! mock would either be too narrow (failing for cross-family scenarios)
//! or too broad (fixed responses that can't model the journal-driven
//! state transitions). This module therefore deliberately offers no
//! topology installer; tests compose with `MockRunner::with_handler`
//! per-call.

use super::shared::PoolFixture;
use crate::cmd::{CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
use crate::config::Config;
use crate::inhibit::{AcquireSleepInhibitor, SleepGuard};
use crate::probe::Filesystem;
use crate::progress::{self, ProgressOutput};
use crate::recover::RecoverParams;
use crate::state_paths::StatePaths;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Default sleep-inhibitor for `RecoverParamsBuilder`. A `'static`
/// reference coerces to any builder lifetime, so the default arm does
/// not depend on a recover-test-mod-local `NOOP_INHIBITOR`. Tests that
/// observe the inhibitor seam (e.g. `RequestCountInhibitor`,
/// `FailingInhibitor`) still pass theirs in via
/// `.sleep_inhibitor(...)`.
struct NoopInhibitor;

impl AcquireSleepInhibitor for NoopInhibitor {
    fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn SleepGuard>> {
        Ok(Box::new(()))
    }
}

static RECOVER_NOOP_INHIBITOR: NoopInhibitor = NoopInhibitor;

impl PoolFixture {
    /// Start a `RecoverParamsBuilder` whose defaults match the most
    /// common recover-test shape: passphrase from the fixture's
    /// pass_path, `dry_run=false`, `allow_degraded=false`,
    /// `progress=Off`, no-op sleep inhibitor. Tests override only the
    /// fields the scenario actually exercises.
    pub(crate) fn recover_params(&self) -> RecoverParamsBuilder<'_> {
        RecoverParamsBuilder {
            config: &self.config,
            paths: &self.paths,
            passphrase_stdin: false,
            passphrase_file: Some(self.pass_path.as_path()),
            allow_degraded: false,
            dry_run: false,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &RECOVER_NOOP_INHIBITOR,
        }
    }
}

/// Per-test `RecoverParams` builder over the fixture defaults. Tests
/// pass a custom `&dyn AcquireSleepInhibitor` via `.sleep_inhibitor(...)`
/// to thread `FailingInhibitor` or `RequestCountInhibitor`; no need to
/// extend the shared `inhibitor` field on `PoolFixture`.
pub(crate) struct RecoverParamsBuilder<'a> {
    config: &'a Config,
    paths: &'a StatePaths,
    passphrase_stdin: bool,
    passphrase_file: Option<&'a Path>,
    allow_degraded: bool,
    dry_run: bool,
    progress: ProgressOutput,
    sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}

impl<'a> RecoverParamsBuilder<'a> {
    pub(crate) fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub(crate) fn allow_degraded(mut self, allow: bool) -> Self {
        self.allow_degraded = allow;
        self
    }

    pub(crate) fn passphrase_file(mut self, path: Option<&'a Path>) -> Self {
        self.passphrase_file = path;
        self
    }

    pub(crate) fn sleep_inhibitor(mut self, inhibitor: &'a dyn AcquireSleepInhibitor) -> Self {
        self.sleep_inhibitor = inhibitor;
        self
    }

    pub(crate) fn build(self) -> RecoverParams<'a> {
        RecoverParams {
            config: self.config,
            paths: self.paths,
            passphrase_stdin: self.passphrase_stdin,
            passphrase_file: self.passphrase_file,
            allow_degraded: self.allow_degraded,
            dry_run: self.dry_run,
            progress: self.progress,
            sleep_inhibitor: self.sleep_inhibitor,
            sleeper: &progress::NoopSleeper,
        }
    }
}

// ---------------------------------------------------------------------------
// RemountHarness (stateful FS + mapper-closing runner)
// ---------------------------------------------------------------------------

type SharedPaths = Arc<Mutex<HashSet<String>>>;
type SharedClosed = Arc<Mutex<HashSet<String>>>;

/// Mock filesystem with interior mutability so test code can model
/// device-mapper paths disappearing when `cryptsetup close` runs and
/// reappearing on `CryptsetupLuksOpen`. Used together with
/// `RemountRunner` for tests that exercise the recover relock cycle on
/// initially-open mappers.
pub(crate) struct RemountFs {
    paths: SharedPaths,
}

impl Filesystem for RemountFs {
    fn exists(&self, path: &str) -> bool {
        self.paths.lock().unwrap().contains(path)
    }
    fn is_block_device(&self, _path: &str) -> bool {
        false
    }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            return Ok(
                "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                    .to_owned(),
            );
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }
}

/// Wraps a `MockRunner` and removes `/dev/mapper/<mapper>` from a
/// shared `RemountFs` whenever a `CryptsetupClose` request succeeds. On
/// `CryptsetupLuksOpen` / `CryptsetupLuksOpenKeyFile` the mapper path
/// is restored. The harness also short-circuits `CryptsetupStatus` for
/// any mapper currently in `closed` *before* delegating to the inner
/// `MockRunner`, so those short-circuited probes are intentionally NOT
/// recorded in `requests()` -- this matches the pre-migration
/// `MapperClosingRunner` behavior and the assertions that depend on it.
pub(crate) struct RemountRunner {
    inner: MockRunner,
    fs_paths: SharedPaths,
    closed: SharedClosed,
}

impl RemountRunner {
    fn inactive_status(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
        }
    }
}

impl CommandRunner for RemountRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, crate::cmd::CmdError> {
        if let CmdRequest::CryptsetupStatus { mapper } = request
            && self.closed.lock().unwrap().contains(mapper.as_str())
        {
            return Ok(Self::inactive_status(mapper.as_str()));
        }
        let result = self.inner.run(request)?;
        match request {
            CmdRequest::CryptsetupClose { mapper } if result.exit_status == 0 => {
                self.fs_paths
                    .lock()
                    .unwrap()
                    .remove(&format!("/dev/mapper/{}", mapper));
                self.closed
                    .lock()
                    .unwrap()
                    .insert(mapper.as_str().to_owned());
            }
            CmdRequest::CryptsetupLuksOpen { mapper, .. }
            | CmdRequest::CryptsetupLuksOpenKeyFile { mapper, .. }
                if result.exit_status == 0 =>
            {
                self.fs_paths
                    .lock()
                    .unwrap()
                    .insert(format!("/dev/mapper/{}", mapper));
                self.closed.lock().unwrap().remove(mapper.as_str());
            }
            _ => {}
        }
        Ok(result)
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, crate::cmd::CmdError> {
        let result = self.inner.run_with_stdin(request, stdin)?;
        match request {
            CmdRequest::CryptsetupLuksOpen { mapper, .. }
            | CmdRequest::CryptsetupLuksOpenKeyFile { mapper, .. }
                if result.exit_status == 0 =>
            {
                self.fs_paths
                    .lock()
                    .unwrap()
                    .insert(format!("/dev/mapper/{}", mapper));
                self.closed.lock().unwrap().remove(mapper.as_str());
            }
            _ => {}
        }
        Ok(result)
    }
}

/// Bundled `RemountFs` + `RemountRunner` so tests can build the
/// stateful FS, hand a fully-configured `MockRunner` to the harness,
/// and observe the wrapped runner with one struct.
///
/// Wraps a fully-built `MockRunner` (with all `with_output` /
/// `with_handler` calls already applied). Tests configure the inner
/// `MockRunner` first, then hand it to `RemountHarness::new`. The
/// returned harness exposes `&self.fs` and `&self.runner` for passing
/// to `cmd_recover` / `plan_recover`, plus `requests()` (delegating to
/// the wrapped runner's request log) for the post-condition assertions
/// the existing tests already make.
pub(crate) struct RemountHarness {
    pub(crate) fs: RemountFs,
    pub(crate) runner: RemountRunner,
    inner_log: MockRunner,
}

impl RemountHarness {
    /// Build the harness. `initial_paths` are seeded into the FS;
    /// `already_closed` are seeded into the closed-mappers set so a
    /// `CryptsetupStatus` probe for those mappers reports inactive
    /// without ever calling into the inner runner. `inner` is a
    /// fully-built `MockRunner` whose request log will be observable
    /// via `requests()`.
    pub(crate) fn new(initial_paths: &[&str], inner: MockRunner, already_closed: &[&str]) -> Self {
        let paths: SharedPaths = Arc::new(Mutex::new(
            initial_paths.iter().map(|s| (*s).to_string()).collect(),
        ));
        let closed: SharedClosed = Arc::new(Mutex::new(
            already_closed.iter().map(|s| (*s).to_string()).collect(),
        ));
        let inner_log = inner.clone();
        Self {
            fs: RemountFs {
                paths: Arc::clone(&paths),
            },
            runner: RemountRunner {
                inner,
                fs_paths: paths,
                closed,
            },
            inner_log,
        }
    }

    /// Snapshot of the wrapped `MockRunner`'s request log. `MockRunner`
    /// pushes to its log *before* dispatch, so every request that
    /// reached the inner runner -- regardless of whether a handler or
    /// `with_output` resolved it -- shows up here. `CryptsetupStatus`
    /// probes that the harness short-circuited for closed mappers do
    /// NOT appear, which matches pre-migration behavior.
    pub(crate) fn requests(&self) -> Vec<CmdRequest> {
        self.inner_log.requests()
    }
}
