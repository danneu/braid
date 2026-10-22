//! `braid-online.service` lifecycle fixups that must run under pool lock.
//!
//! These operations used to live in the shell wrapper. Keeping them behind a
//! Rust seam lets dispatch hold `/run/braid-pool.lock` through mount
//! permissions and systemd lifecycle changes.

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::Config;
use crate::filesystem::Filesystem;
use crate::pool_mount::{self, MountedAt, Mountpoint, PoolMounted, PoolUnmounted};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use crate::util::detail_suffix;
use nix::unistd::{Group, User, chown};
#[cfg(test)]
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use thiserror::Error;

pub const BRAID_ONLINE_UNIT: &str = "braid-online.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitActiveState {
    Active,
    Activating,
    Deactivating,
    Inactive,
    Failed,
    Maintenance,
    Reloading,
    Refreshing,
    Unknown(String),
}

impl UnitActiveState {
    fn parse(raw: &str) -> Self {
        match raw.trim() {
            "active" => Self::Active,
            "activating" => Self::Activating,
            "deactivating" => Self::Deactivating,
            "inactive" => Self::Inactive,
            "failed" => Self::Failed,
            "maintenance" => Self::Maintenance,
            "reloading" => Self::Reloading,
            "refreshing" => Self::Refreshing,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Canonical systemd word for known variants; the captured reason text for
    /// `Unknown` so parser and user-facing diagnostics render from one mapping.
    pub fn systemd_word(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Activating => "activating",
            Self::Deactivating => "deactivating",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Maintenance => "maintenance",
            Self::Reloading => "reloading",
            Self::Refreshing => "refreshing",
            Self::Unknown(reason) => reason.as_str(),
        }
    }
}

#[derive(Debug, Error)]
pub enum OnlineError {
    #[error("{source}")]
    Spawn { source: CmdError },
    #[error("systemctl show {unit} failed (exit {exit_code}){}", detail_suffix(.stderr))]
    SystemctlShow {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    #[error("mountpoint check for {path} failed (exit {exit_code}){}", detail_suffix(.stderr))]
    Mountpoint {
        path: String,
        exit_code: i32,
        stderr: String,
    },
    #[error("user {user} not found")]
    UserNotFound { user: String },
    #[error("group {group} not found")]
    GroupNotFound { group: String },
    #[error("failed to resolve user/group: {0}")]
    UserGroup(#[from] nix::errno::Errno),
    #[error("failed to chown {path}: {source}")]
    Chown {
        path: String,
        source: nix::errno::Errno,
    },
    #[error("failed to chmod {path}: {source}")]
    Chmod {
        path: String,
        source: std::io::Error,
    },
    #[error("systemctl start {unit} failed (exit {exit_code}){}", detail_suffix(.stderr))]
    SystemctlStart {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    #[error("systemctl stop {unit} failed (exit {exit_code}){}", detail_suffix(.stderr))]
    SystemctlStop {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
}

pub trait OnlineStateOps {
    fn unit_active_state(&self, unit: &str) -> Result<UnitActiveState, OnlineError>;
    fn is_mountpoint(&self, mount_point: &MountPoint) -> Result<bool, OnlineError>;
    fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<(), OnlineError>;
    fn chmod(&self, path: &Path, mode: u32) -> Result<(), OnlineError>;
    fn systemctl_start(&self, unit: &str) -> Result<(), OnlineError>;
    fn systemctl_stop(&self, unit: &str, no_block: bool) -> Result<(), OnlineError>;
    fn list_bound_by(&self, unit: &str) -> Result<Vec<String>, OnlineError>;
}

/// Production implementation of lifecycle checks and fixups.
pub struct RealOnlineStateOps<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> RealOnlineStateOps<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }
}

impl OnlineStateOps for RealOnlineStateOps<'_> {
    fn unit_active_state(&self, unit: &str) -> Result<UnitActiveState, OnlineError> {
        let output = self
            .runner
            .run(&CmdRequest::SystemctlShowActiveState { unit: unit.into() })
            .map_err(|source| OnlineError::Spawn { source })?;
        if output.exit_status != 0 {
            return Err(OnlineError::SystemctlShow {
                unit: unit.into(),
                exit_code: output.exit_status,
                stderr: output.stderr.trim().into(),
            });
        }
        Ok(UnitActiveState::parse(&output.stdout))
    }

    fn is_mountpoint(&self, mount_point: &MountPoint) -> Result<bool, OnlineError> {
        let output = self
            .runner
            .run(&CmdRequest::MountpointCheck {
                path: mount_point.clone().into(),
            })
            .map_err(|source| OnlineError::Spawn { source })?;
        match output.exit_status {
            0 => Ok(true),
            32 => Ok(false),
            code => Err(OnlineError::Mountpoint {
                path: mount_point.as_str().to_owned(),
                exit_code: code,
                stderr: output.stderr.trim().into(),
            }),
        }
    }

    fn chown(&self, path: &Path, owner: &str, group: &str) -> Result<(), OnlineError> {
        let user = User::from_name(owner)?
            .ok_or_else(|| OnlineError::UserNotFound { user: owner.into() })?;
        let group = Group::from_name(group)?.ok_or_else(|| OnlineError::GroupNotFound {
            group: group.into(),
        })?;
        chown(path, Some(user.uid), Some(group.gid)).map_err(|source| OnlineError::Chown {
            path: path.display().to_string(),
            source,
        })
    }

    fn chmod(&self, path: &Path, mode: u32) -> Result<(), OnlineError> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
            OnlineError::Chmod {
                path: path.display().to_string(),
                source,
            }
        })
    }

    fn systemctl_start(&self, unit: &str) -> Result<(), OnlineError> {
        let output = self
            .runner
            .run(&CmdRequest::SystemctlStart { unit: unit.into() })
            .map_err(|source| OnlineError::Spawn { source })?;
        if output.exit_status == 0 {
            Ok(())
        } else {
            Err(OnlineError::SystemctlStart {
                unit: unit.into(),
                exit_code: output.exit_status,
                stderr: output.stderr.trim().into(),
            })
        }
    }

    fn systemctl_stop(&self, unit: &str, no_block: bool) -> Result<(), OnlineError> {
        let output = self
            .runner
            .run(&CmdRequest::SystemctlStop {
                unit: unit.into(),
                no_block,
            })
            .map_err(|source| OnlineError::Spawn { source })?;
        if output.exit_status == 0 {
            Ok(())
        } else {
            Err(OnlineError::SystemctlStop {
                unit: unit.into(),
                exit_code: output.exit_status,
                stderr: output.stderr.trim().into(),
            })
        }
    }

    fn list_bound_by(&self, unit: &str) -> Result<Vec<String>, OnlineError> {
        let output = self
            .runner
            .run(&CmdRequest::SystemctlShowBoundBy { unit: unit.into() })
            .map_err(|source| OnlineError::Spawn { source })?;
        if output.exit_status != 0 {
            return Err(OnlineError::SystemctlShow {
                unit: unit.into(),
                exit_code: output.exit_status,
                stderr: output.stderr.trim().into(),
            });
        }
        Ok(output
            .stdout
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }
}

/// Entry-state `braid-online.service` ActiveState captured inside locked dispatch for `mark_online`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineSnapshot {
    pub online_state: UnitActiveState,
}

/// Captures the `braid-online.service` entry state used by `mark_online` after mutation.
/// Must run at the start of the pool-lock window; see the snapshot rule in
/// docs/design/decisions/026-pool-lock-rust-owned.md.
pub fn snapshot(ops: &dyn OnlineStateOps) -> OnlineSnapshot {
    let online_state = ops
        .unit_active_state(BRAID_ONLINE_UNIT)
        .unwrap_or_else(|e| UnitActiveState::Unknown(e.to_string()));
    OnlineSnapshot { online_state }
}

/// Brings a mounted pool online: mount-root ownership, then the
/// `braid-online.service` start. It takes the `PoolMounted` proof so it never
/// acts on a mount that is not the pool. Uses this lock window's entry-state
/// `OnlineSnapshot` to gate the start; skipping captured
/// active/activating/deactivating states avoids queueing start behind stop.
/// See the snapshot rule in docs/design/decisions/026-pool-lock-rust-owned.md.
pub fn mark_online(
    mounted: PoolMounted,
    snap: Option<&OnlineSnapshot>,
    cfg: &Config,
    ops: &dyn OnlineStateOps,
) {
    mark_online_to(mounted, snap, cfg, ops, &mut std::io::stderr());
}

/// Implements the online marker against an injected warning sink so its
/// warn-and-continue contract is behaviorally testable without redirecting
/// process-global stderr.
fn mark_online_to(
    _mounted: PoolMounted,
    snap: Option<&OnlineSnapshot>,
    cfg: &Config,
    ops: &dyn OnlineStateOps,
    out: &mut dyn Write,
) {
    let mount_point = Path::new(cfg.mount_point().as_str());
    if let Some(group) = cfg.pool_access_group() {
        if let Err(e) = ops.chown(mount_point, "root", group) {
            writeln!(
                out,
                "braid: WARNING: failed to set ownership on {}: {e}",
                mount_point.display()
            )
            .ok();
        }
        if let Err(e) = ops.chmod(mount_point, 0o2770) {
            writeln!(
                out,
                "braid: WARNING: failed to set permissions on {}: {e}",
                mount_point.display()
            )
            .ok();
        }
    }

    if cfg.systemd_lifecycle()
        && let Some(snap) = snap
    {
        match &snap.online_state {
            UnitActiveState::Inactive | UnitActiveState::Failed => {
                if let Err(e) = ops.systemctl_start(BRAID_ONLINE_UNIT) {
                    writeln!(
                        out,
                        "braid: WARNING: failed to activate braid-online.service ({e}) -- pool is mounted but shutdown may not lock automatically"
                    )
                    .ok();
                }
            }
            UnitActiveState::Unknown(reason) => {
                writeln!(
                    out,
                    "braid: WARNING: could not read braid-online.service ActiveState ({reason}) -- pool is mounted but shutdown may not lock automatically"
                )
                .ok();
            }
            UnitActiveState::Active
            | UnitActiveState::Activating
            | UnitActiveState::Deactivating
            | UnitActiveState::Maintenance
            | UnitActiveState::Reloading
            | UnitActiveState::Refreshing => {}
        }
    }
}

/// Shared online-side finalizer so dispatch cannot skip lifecycle
/// reconciliation after post-mount command failures. It looks at the mount
/// only after `op` returns, under the pool lock dispatch still holds, so it
/// reads the `pool.json` and journal `op` left behind -- a bootstrap add's new
/// pool is known by then.
pub fn run_with_online_marker<E>(
    snap: Option<&OnlineSnapshot>,
    cfg: Option<&Config>,
    paths: &StatePaths,
    ops: &dyn OnlineStateOps,
    fs: &dyn Filesystem,
    op: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    run_with_online_marker_to(snap, cfg, paths, ops, fs, &mut std::io::stderr(), op)
}

/// Runs the online finalizer with an injected warning sink while preserving
/// the wrapped operation's result as the only return value.
fn run_with_online_marker_to<E>(
    snap: Option<&OnlineSnapshot>,
    cfg: Option<&Config>,
    paths: &StatePaths,
    ops: &dyn OnlineStateOps,
    fs: &dyn Filesystem,
    out: &mut dyn Write,
    op: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let result = op();
    if let Some(cfg) = cfg {
        finalize_online_to(snap, cfg, paths, ops, fs, out);
    }
    result
}

/// The finalizer body against an injected warning sink: mark the pool online
/// only on a `PoolMounted` proof. Nothing mounted is the quiet pre-mount
/// failure; a failed check, a foreign mount, or an unproven one warns and
/// changes nothing.
fn finalize_online_to(
    snap: Option<&OnlineSnapshot>,
    cfg: &Config,
    paths: &StatePaths,
    ops: &dyn OnlineStateOps,
    fs: &dyn Filesystem,
    out: &mut dyn Write,
) {
    let mount = match pool_mount::check_mountpoint(ops, cfg.mount_point()) {
        Ok(Mountpoint::Mounted(mount)) => mount,
        Ok(Mountpoint::NotMounted(_)) => return,
        Err(e) => {
            writeln!(
                out,
                "braid: WARNING: failed to check mountpoint {:?}: {e}",
                Path::new(cfg.mount_point().as_str())
            )
            .ok();
            return;
        }
    };
    let skipped = match mount.classify(fs, paths) {
        MountedAt::Pool(mounted) => return mark_online_to(mounted, snap, cfg, ops, out),
        MountedAt::Foreign(foreign) => foreign.to_string(),
        MountedAt::Unknown(unproven) => unproven.to_string(),
    };
    writeln!(
        out,
        "braid: WARNING: {skipped} -- not changing its ownership or starting braid-online.service"
    )
    .ok();
}

/// Plain lock's offline gate, probed once after each lock attempt whether it
/// succeeded or failed. An unknown mount state counts as still mounted --
/// mirroring `run_with_online_marker`, which warns and skips on a failed
/// check -- so a probe error warns and returns no proof.
pub fn observe_unmounted(cfg: &Config, ops: &dyn OnlineStateOps) -> Option<PoolUnmounted> {
    observe_unmounted_to(cfg, ops, &mut std::io::stderr())
}

/// Implements the offline gate against an injected warning sink for the same
/// testable warn-and-continue boundary as `mark_online_to`.
fn observe_unmounted_to(
    cfg: &Config,
    ops: &dyn OnlineStateOps,
    out: &mut dyn Write,
) -> Option<PoolUnmounted> {
    match pool_mount::check_mountpoint(ops, cfg.mount_point()) {
        Ok(Mountpoint::Mounted(_)) => None,
        Ok(Mountpoint::NotMounted(unmounted)) => Some(unmounted),
        Err(e) => {
            writeln!(
                out,
                "braid: WARNING: failed to check mountpoint {}: {e}",
                cfg.mount_point().as_str()
            )
            .ok();
            None
        }
    }
}

/// Plain lock's offline marker: stop does not use the online snapshot gate.
/// It relies on `/run/braid-stop-coordinator.lock` and the `done\n` protocol from
/// docs/design/decisions/026-pool-lock-rust-owned.md instead.
pub fn mark_offline(unmounted: PoolUnmounted, cfg: &Config, ops: &dyn OnlineStateOps) {
    mark_offline_to(unmounted, cfg, ops, &mut std::io::stderr());
}

/// Implements the offline marker against an injected warning sink for the
/// same testable warn-and-continue boundary as `mark_online_to`.
fn mark_offline_to(
    _unmounted: PoolUnmounted,
    cfg: &Config,
    ops: &dyn OnlineStateOps,
    out: &mut dyn Write,
) {
    if cfg.systemd_lifecycle()
        && let Err(e) = ops.systemctl_stop(BRAID_ONLINE_UNIT, false)
    {
        writeln!(
            out,
            "braid: WARNING: failed to deactivate braid-online.service: {e}"
        )
        .ok();
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub enum StagedOnlineFailure {
    Spawn(String),
    SystemctlShow {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    SystemctlStart {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    SystemctlStop {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
}

#[cfg(test)]
impl StagedOnlineFailure {
    fn into_online_error(self) -> OnlineError {
        match self {
            Self::Spawn(msg) => OnlineError::Spawn {
                source: CmdError::Failed(msg),
            },
            Self::SystemctlShow {
                unit,
                exit_code,
                stderr,
            } => OnlineError::SystemctlShow {
                unit,
                exit_code,
                stderr,
            },
            Self::SystemctlStart {
                unit,
                exit_code,
                stderr,
            } => OnlineError::SystemctlStart {
                unit,
                exit_code,
                stderr,
            },
            Self::SystemctlStop {
                unit,
                exit_code,
                stderr,
            } => OnlineError::SystemctlStop {
                unit,
                exit_code,
                stderr,
            },
        }
    }
}

#[cfg(test)]
pub struct RecordingOnlineStateOps {
    state: std::cell::RefCell<Result<UnitActiveState, String>>,
    mounted: std::cell::RefCell<Result<bool, StagedOnlineFailure>>,
    calls: std::cell::RefCell<Vec<String>>,
    bound_by: std::cell::RefCell<Result<Vec<String>, StagedOnlineFailure>>,
    systemctl_start_errs: std::cell::RefCell<HashMap<String, StagedOnlineFailure>>,
    systemctl_stop_errs: std::cell::RefCell<HashMap<String, StagedOnlineFailure>>,
    coord_file_path: Option<std::path::PathBuf>,
    coord_snapshots: std::cell::RefCell<Vec<Vec<u8>>>,
}

#[cfg(test)]
impl RecordingOnlineStateOps {
    pub fn new() -> Self {
        Self {
            state: std::cell::RefCell::new(Ok(UnitActiveState::Inactive)),
            mounted: std::cell::RefCell::new(Ok(true)),
            calls: std::cell::RefCell::new(Vec::new()),
            bound_by: std::cell::RefCell::new(Ok(Vec::new())),
            systemctl_start_errs: std::cell::RefCell::new(HashMap::new()),
            systemctl_stop_errs: std::cell::RefCell::new(HashMap::new()),
            coord_file_path: None,
            coord_snapshots: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Opt-in instrumentation for plain-lock orchestration tests; snapshots the
    /// coordinator marker file on every `braid-online.service` stop.
    pub fn with_coord_file(mut self, path: std::path::PathBuf) -> Self {
        self.coord_file_path = Some(path);
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    pub fn coord_snapshots(&self) -> Vec<Vec<u8>> {
        self.coord_snapshots.borrow().clone()
    }

    pub fn set_mounted(&self, mounted: bool) {
        *self.mounted.borrow_mut() = Ok(mounted);
    }

    pub fn set_mountpoint_err(&self, failure: StagedOnlineFailure) {
        *self.mounted.borrow_mut() = Err(failure);
    }

    pub fn set_bound_by_ok(&self, units: Vec<String>) {
        *self.bound_by.borrow_mut() = Ok(units);
    }

    pub fn set_bound_by_err(&self, failure: StagedOnlineFailure) {
        *self.bound_by.borrow_mut() = Err(failure);
    }

    /// Stages a start failure for lifecycle-warning and finalizer tests.
    pub fn set_systemctl_start_err(&self, unit: &str, failure: StagedOnlineFailure) {
        self.systemctl_start_errs
            .borrow_mut()
            .insert(unit.to_owned(), failure);
    }

    pub fn set_systemctl_stop_err(&self, unit: &str, failure: StagedOnlineFailure) {
        self.systemctl_stop_errs
            .borrow_mut()
            .insert(unit.to_owned(), failure);
    }
}

#[cfg(test)]
impl Default for RecordingOnlineStateOps {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl OnlineStateOps for RecordingOnlineStateOps {
    fn unit_active_state(&self, _unit: &str) -> Result<UnitActiveState, OnlineError> {
        self.state.borrow().clone().map_err(|s| OnlineError::Spawn {
            source: CmdError::Failed(s),
        })
    }

    fn is_mountpoint(&self, _mount_point: &MountPoint) -> Result<bool, OnlineError> {
        self.calls.borrow_mut().push("mountpoint".into());
        self.mounted
            .borrow()
            .clone()
            .map_err(StagedOnlineFailure::into_online_error)
    }

    fn chown(&self, _path: &Path, _owner: &str, _group: &str) -> Result<(), OnlineError> {
        self.calls.borrow_mut().push("chown".into());
        Ok(())
    }

    fn chmod(&self, _path: &Path, _mode: u32) -> Result<(), OnlineError> {
        self.calls.borrow_mut().push("chmod".into());
        Ok(())
    }

    fn systemctl_start(&self, unit: &str) -> Result<(), OnlineError> {
        self.calls.borrow_mut().push(format!("start {unit}"));
        match self.systemctl_start_errs.borrow().get(unit).cloned() {
            Some(failure) => Err(failure.into_online_error()),
            None => Ok(()),
        }
    }

    fn systemctl_stop(&self, unit: &str, no_block: bool) -> Result<(), OnlineError> {
        self.calls
            .borrow_mut()
            .push(format!("stop {unit} no_block={no_block}"));
        if unit == BRAID_ONLINE_UNIT
            && let Some(path) = &self.coord_file_path
        {
            let bytes = fs::read(path).unwrap_or_default();
            self.coord_snapshots.borrow_mut().push(bytes);
        }
        match self.systemctl_stop_errs.borrow().get(unit).cloned() {
            Some(failure) => Err(failure.into_online_error()),
            None => Ok(()),
        }
    }

    fn list_bound_by(&self, unit: &str) -> Result<Vec<String>, OnlineError> {
        self.calls
            .borrow_mut()
            .push(format!("list_bound_by {unit}"));
        self.bound_by
            .borrow()
            .clone()
            .map_err(StagedOnlineFailure::into_online_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::membership::save_membership;
    use crate::test_fixtures::{MockFs, isolated_paths, two_disk_membership};
    use tempfile::TempDir;

    fn cfg(raw: &str) -> Config {
        serde_json::from_str(raw).expect("config should parse")
    }

    fn out(stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "systemctl".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status,
        }
    }

    fn mountpoint_out(exit_status: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "mountpoint -q /mnt/storage".into(),
            stdout: String::new(),
            stderr: stderr.into(),
            exit_status,
        }
    }

    // Intent: RealOnlineStateOps maps mountpoint exit 0 to mounted.
    // Why it exists: the util-linux mountpoint exit-code contract is
    // load-bearing for both UPS shutdown safety and cached-path skip messages;
    // regressing it would silently corrupt diagnostics.
    // Scenario: `mountpoint -q /mnt/storage` confirms the pool is mounted.
    #[test]
    fn real_ops_mountpoint_exit_zero_is_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            mountpoint_out(0, ""),
        );
        let ops = RealOnlineStateOps::new(&runner);

        assert!(
            ops.is_mountpoint(&MountPoint::parse("/mnt/storage").unwrap())
                .unwrap()
        );
    }

    // Intent: RealOnlineStateOps maps mountpoint exit 32 to not mounted.
    // Why it exists: the util-linux mountpoint exit-code contract is
    // load-bearing for both UPS shutdown safety and cached-path skip messages;
    // regressing it would silently corrupt diagnostics.
    // Scenario: `mountpoint -q /mnt/storage` sees an existing directory that
    // is not a mountpoint.
    #[test]
    fn real_ops_mountpoint_exit_32_is_not_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            mountpoint_out(32, "/mnt/storage is not a mountpoint\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        assert!(
            !ops.is_mountpoint(&MountPoint::parse("/mnt/storage").unwrap())
                .unwrap()
        );
    }

    // Intent: RealOnlineStateOps treats mountpoint exit 1 as a probe error.
    // Why it exists: the util-linux mountpoint exit-code contract is
    // load-bearing for both UPS shutdown safety and cached-path skip messages;
    // regressing it would silently corrupt diagnostics.
    // Scenario: `mountpoint -q` is invoked incorrectly or cannot stat the path.
    #[test]
    fn real_ops_mountpoint_exit_one_is_error() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            mountpoint_out(1, "bad usage\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        let err = ops
            .is_mountpoint(&MountPoint::parse("/mnt/storage").unwrap())
            .expect_err("exit 1 should be classified as a probe error");
        assert!(
            matches!(err, OnlineError::Mountpoint { exit_code: 1, .. }),
            "got: {err:?}"
        );
    }

    // Intent: RealOnlineStateOps treats unexpected mountpoint exits as probe errors.
    // Why it exists: the util-linux mountpoint exit-code contract is
    // load-bearing for both UPS shutdown safety and cached-path skip messages;
    // regressing it would silently corrupt diagnostics.
    // Scenario: a future or wrapper-specific `mountpoint(1)` failure exits with
    // a non-contract code that must not be mistaken for "not mounted".
    #[test]
    fn real_ops_mountpoint_other_exit_is_error() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            mountpoint_out(2, "unexpected failure\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        let err = ops
            .is_mountpoint(&MountPoint::parse("/mnt/storage").unwrap())
            .expect_err("unexpected exit should be classified as a probe error");
        assert!(
            matches!(err, OnlineError::Mountpoint { exit_code: 2, .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn parses_active_state_refreshing() {
        let runner = MockRunner::default().with_output(
            CmdRequest::SystemctlShowActiveState {
                unit: BRAID_ONLINE_UNIT.into(),
            },
            out("refreshing\n", 0),
        );
        let ops = RealOnlineStateOps::new(&runner);
        assert_eq!(
            ops.unit_active_state(BRAID_ONLINE_UNIT).unwrap(),
            UnitActiveState::Refreshing
        );
    }

    #[test]
    fn list_bound_by_parses_whitespace_separated_units() {
        let runner = MockRunner::default().with_output(
            CmdRequest::SystemctlShowBoundBy {
                unit: BRAID_ONLINE_UNIT.into(),
            },
            out("smbd.service nfs-server.service\n", 0),
        );
        let ops = RealOnlineStateOps::new(&runner);
        assert_eq!(
            ops.list_bound_by(BRAID_ONLINE_UNIT).unwrap(),
            vec!["smbd.service", "nfs-server.service"]
        );
    }

    /// The LUKS UUID `two_disk_membership` gives disk1, recorded on
    /// `braid-disk1`, the device `MockFs::storage`'s mountinfo names.
    const POOL_UUID: &str = "11111111-1111-1111-1111-111111111111";

    /// A state dir and filesystem where the mount at /mnt/storage classifies
    /// as this pool. The `TempDir` keeps the state dir alive.
    fn pool_at_mount_point() -> (TempDir, StatePaths, MockFs) {
        let (dir, paths) = isolated_paths();
        save_membership(&two_disk_membership(), &paths).unwrap();
        let fs = MockFs::storage(vec![]).with_luks_dm("braid-disk1", POOL_UUID);
        (dir, paths, fs)
    }

    /// Run the finalizer body over the pool fixture; returns its warnings.
    fn finalize(
        snap: Option<&OnlineSnapshot>,
        cfg: &Config,
        ops: &RecordingOnlineStateOps,
    ) -> String {
        let (_dir, paths, fs) = pool_at_mount_point();
        let mut out = Vec::new();
        finalize_online_to(snap, cfg, &paths, ops, &fs, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn inactive() -> OnlineSnapshot {
        OnlineSnapshot {
            online_state: UnitActiveState::Inactive,
        }
    }

    /// The unmounted proof, built the only way production builds it.
    fn unmounted_proof() -> PoolUnmounted {
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);
        match pool_mount::check_mountpoint(&ops, &MountPoint::new("/mnt/storage".into())) {
            Ok(Mountpoint::NotMounted(proof)) => proof,
            other => panic!("expected the unmounted proof, got {other:?}"),
        }
    }

    // Intent: mark_online skips braid-online.service when lifecycle is disabled.
    // Why it exists: standalone CLI configs still run mount fixups but do not
    // own the module-defined online unit.
    // Scenario: `braid add` succeeds in a CLI-only test VM with no
    // braid-online.service installed.
    #[test]
    fn mark_online_skips_systemctl_when_lifecycle_disabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage"}"#);
        let ops = RecordingOnlineStateOps::new();

        finalize(Some(&inactive()), &cfg, &ops);

        let calls = ops.calls();
        assert!(calls.contains(&"mountpoint".into()));
        assert!(!calls.contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: mark_online keeps pool access permissions independent of lifecycle.
    // Why it exists: module-generated configs use the same post-mount path for
    // permissions, and standalone configs may still opt into the JSON field.
    // Scenario: pool is mounted, pool_access_group is configured, but systemd
    // lifecycle is absent.
    #[test]
    fn mark_online_applies_pool_access_group_without_lifecycle() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","pool_access_group":"storage"}"#);
        let ops = RecordingOnlineStateOps::new();

        finalize(Some(&inactive()), &cfg, &ops);

        let calls = ops.calls();
        assert!(calls.contains(&"chown".into()));
        assert!(calls.contains(&"chmod".into()));
        assert!(!calls.contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    #[test]
    fn mark_online_starts_when_lifecycle_enabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        for state in [UnitActiveState::Inactive, UnitActiveState::Failed] {
            let ops = RecordingOnlineStateOps::new();
            finalize(
                Some(&OnlineSnapshot {
                    online_state: state,
                }),
                &cfg,
                &ops,
            );
            assert!(ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
        }

        for state in [UnitActiveState::Deactivating, UnitActiveState::Maintenance] {
            let ops = RecordingOnlineStateOps::new();
            finalize(
                Some(&OnlineSnapshot {
                    online_state: state,
                }),
                &cfg,
                &ops,
            );
            assert!(!ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
        }
    }

    // Intent: a failed braid-online start reports systemctl's exit code and
    // diagnostic while preserving the wrapped command's original result.
    // Why it exists: the lifecycle finalizer used to reduce OnlineError to a
    // boolean, hiding the diagnostic needed to repair automatic shutdown.
    // Scenario: add mounts the pool and then fails; its finalizer also cannot
    // start braid-online.service because systemd rejects the request.
    #[test]
    fn online_finalizer_warns_with_start_failure_details_and_preserves_result() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_start_err(
            BRAID_ONLINE_UNIT,
            StagedOnlineFailure::SystemctlStart {
                unit: BRAID_ONLINE_UNIT.into(),
                exit_code: 5,
                stderr: "Unit braid-online.service not loaded.".into(),
            },
        );
        let (_dir, paths, fs) = pool_at_mount_point();
        let mut out = Vec::new();

        let result = run_with_online_marker_to(
            Some(&inactive()),
            Some(&cfg),
            &paths,
            &ops,
            &fs,
            &mut out,
            || Err::<(), _>("post-mount failure"),
        );

        assert_eq!(result, Err("post-mount failure"));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "braid: WARNING: failed to activate braid-online.service (systemctl start braid-online.service failed (exit 5): Unit braid-online.service not loaded.) -- pool is mounted but shutdown may not lock automatically\n"
        );
    }

    // Intent: a failed braid-online start with empty stderr still reports the
    // exit code without rendering a contentless diagnostic separator.
    // Why it exists: systemctl is not guaranteed to write stderr on every
    // nonzero exit, and dangling punctuation makes the warning look truncated.
    // Scenario: systemctl start exits 5 silently after the pool is mounted.
    #[test]
    fn mark_online_start_failure_omits_empty_stderr_suffix() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_start_err(
            BRAID_ONLINE_UNIT,
            StagedOnlineFailure::SystemctlStart {
                unit: BRAID_ONLINE_UNIT.into(),
                exit_code: 5,
                stderr: String::new(),
            },
        );
        let snap = OnlineSnapshot {
            online_state: UnitActiveState::Failed,
        };

        let warnings = finalize(Some(&snap), &cfg, &ops);

        assert_eq!(
            warnings,
            "braid: WARNING: failed to activate braid-online.service (systemctl start braid-online.service failed (exit 5)) -- pool is mounted but shutdown may not lock automatically\n"
        );
    }

    // Intent: mark_online tolerates a missing snapshot even when lifecycle is enabled.
    // Why it exists: callers gate snapshot collection separately; forgetting one
    // must not retroactively start braid-online from stale state.
    // Scenario: future dispatch loads module-managed config but skips the
    // pre-command ActiveState snapshot.
    #[test]
    fn mark_online_skips_systemctl_when_snapshot_absent() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();

        finalize(None, &cfg, &ops);

        assert!(!ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: the online finalizer starts braid-online.service even when the
    // wrapped operation returns an error after mounting.
    // Why it exists: bootstrap add and recover can mount successfully, then
    // fail during post-mount persistence or cleanup.
    // Scenario: a post-mount command error returns to dispatch while the pool
    // is mounted and braid-online.service was inactive at command start.
    #[test]
    fn run_with_online_marker_calls_mark_online_on_err() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        let (_dir, paths, fs) = pool_at_mount_point();

        let result =
            run_with_online_marker(Some(&inactive()), Some(&cfg), &paths, &ops, &fs, || {
                Err::<(), _>("post-mount failure")
            });

        assert_eq!(result, Err("post-mount failure"));
        assert!(ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: the online finalizer preserves the success path.
    // Why it exists: routing dispatch through the shared helper must not lose
    // the existing successful mount lifecycle update.
    // Scenario: a pool-touching command succeeds after mounting while
    // braid-online.service was inactive at command start.
    #[test]
    fn run_with_online_marker_calls_mark_online_on_ok() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        let (_dir, paths, fs) = pool_at_mount_point();

        let result =
            run_with_online_marker(Some(&inactive()), Some(&cfg), &paths, &ops, &fs, || {
                Ok::<(), &'static str>(())
            });

        assert_eq!(result, Ok(()));
        assert!(ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: the online finalizer relies on the mountpoint gate on failure
    // paths, and stays quiet when nothing is mounted.
    // Why it exists: pre-mount command errors must not activate
    // braid-online.service for an offline pool.
    // Scenario: planning or credential verification fails before anything is
    // mounted, then dispatch still runs the finalizer.
    #[test]
    fn run_with_online_marker_skips_when_mountpoint_false() {
        let cfg = cfg(
            r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true,"pool_access_group":"storage"}"#,
        );
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);
        let (_dir, paths, fs) = pool_at_mount_point();
        let mut out = Vec::new();

        let result = run_with_online_marker_to(
            Some(&inactive()),
            Some(&cfg),
            &paths,
            &ops,
            &fs,
            &mut out,
            || Err::<(), _>("pre-mount"),
        );

        assert_eq!(result, Err("pre-mount"));
        assert_eq!(ops.calls(), vec!["mountpoint".to_owned()]);
        assert!(out.is_empty(), "got {:?}", String::from_utf8(out));
    }

    // Intent: dry-run dispatch bypasses online lifecycle reconciliation.
    // Why it exists: dry-run commands may share command plumbing but must not
    // probe mountpoints or touch systemd state.
    // Scenario: dispatch passes no config to the finalizer for a dry-run
    // command that returns an error.
    #[test]
    fn run_with_online_marker_skips_when_config_none() {
        let ops = RecordingOnlineStateOps::new();
        let (_dir, paths, fs) = pool_at_mount_point();

        let result = run_with_online_marker(Some(&inactive()), None, &paths, &ops, &fs, || {
            Err::<(), _>("dry-run failure")
        });

        assert_eq!(result, Err("dry-run failure"));
        assert!(ops.calls().is_empty());
    }

    // Intent: the finalizer over a mount that is not the pool changes no
    //   ownership and starts nothing, warns, and returns the command's result.
    // Why it exists: it used to gate only on `mountpoint -q`, so after any
    //   unlock, add, or recover it chowned a foreign mount root to
    //   root:<poolAccessGroup> 2770 and started braid-online.service, whose
    //   consumers then served the wrong filesystem.
    // Scenario: a tmpfs, an ext4 disk, or another btrfs sits at /mnt/storage
    //   and `braid add` refuses it; dispatch still runs the finalizer.
    #[test]
    fn finalizer_leaves_a_foreign_mount_alone() {
        let cfg = cfg(
            r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true,"pool_access_group":"storage"}"#,
        );
        let (_dir, paths, pool_fs) = pool_at_mount_point();
        let foreign = [
            (
                pool_fs.with_mountinfo(
                    "36 35 0:32 / /mnt/storage rw shared:1 - tmpfs tmpfs rw,size=1G\n",
                ),
                "braid: WARNING: /mnt/storage is mounted but fstype is tmpfs, not btrfs -- not changing its ownership or starting braid-online.service\n".to_owned(),
            ),
            (
                MockFs::storage(vec![])
                    .with_mountinfo(
                        "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/backup rw\n",
                    )
                    .with_luks_dm("backup", "99999999-9999-9999-9999-999999999999"),
                "braid: WARNING: /mnt/storage holds a btrfs filesystem on /dev/mapper/backup that is not this pool: its LUKS UUID 99999999-9999-9999-9999-999999999999 is not a member in pool.json or the pending-operation journal -- not changing its ownership or starting braid-online.service\n".to_owned(),
            ),
        ];
        for (fs, expected) in foreign {
            let ops = RecordingOnlineStateOps::new();
            let mut out = Vec::new();

            let result = run_with_online_marker_to(
                Some(&inactive()),
                Some(&cfg),
                &paths,
                &ops,
                &fs,
                &mut out,
                || Err::<(), _>("foreign mount refused"),
            );

            assert_eq!(result, Err("foreign mount refused"));
            assert_eq!(ops.calls(), vec!["mountpoint".to_owned()]);
            assert_eq!(String::from_utf8(out).unwrap(), expected);
        }
    }

    // Intent: a corrupt pool.json at finalizer time warns, changes nothing,
    //   and returns the command's result unchanged; the finalizer never exits.
    // Why it exists: the finalizer reads pool.json itself; an exit or a
    //   panic there would replace the command's own error or exit code.
    // Scenario: unlock fails, the pool is mounted, and pool.json is corrupt.
    #[test]
    fn finalizer_warns_on_a_corrupt_pool_json_and_keeps_the_result() {
        let cfg = cfg(
            r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true,"pool_access_group":"storage"}"#,
        );
        let (_dir, paths, fs) = pool_at_mount_point();
        std::fs::write(paths.pool_json(), "{not json").unwrap();
        let ops = RecordingOnlineStateOps::new();
        let mut out = Vec::new();

        let result = run_with_online_marker_to(
            Some(&inactive()),
            Some(&cfg),
            &paths,
            &ops,
            &fs,
            &mut out,
            || Err::<(), _>("unlock failure"),
        );

        assert_eq!(result, Err("unlock failure"));
        assert_eq!(ops.calls(), vec!["mountpoint".to_owned()]);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.starts_with(
                "braid: WARNING: cannot tell whether the filesystem at /mnt/storage is this pool: pool membership file corrupt at "
            ) && out.ends_with(" -- not changing its ownership or starting braid-online.service\n"),
            "got {out:?}"
        );
    }

    // Intent: a bootstrap add still gets the chown, the chmod, and the
    //   braid-online start, whether it succeeds or fails after mounting.
    // Why it exists: the finalizer reads pool.json after the command returns;
    //   read before it, a bootstrap add has no pool.json and its new pool
    //   would look foreign every time.
    // Scenario: no pool.json exists when dispatch starts; `braid add` mounts
    //   the new pool and writes pool.json, then returns Ok, or Err from a
    //   later step.
    #[test]
    fn finalizer_marks_a_bootstrap_add_online() {
        let cfg = cfg(
            r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true,"pool_access_group":"storage"}"#,
        );
        for result in [Ok(()), Err("post-mount failure")] {
            let (_dir, paths) = isolated_paths();
            let fs = MockFs::storage(vec![]).with_luks_dm("braid-disk1", POOL_UUID);
            let ops = RecordingOnlineStateOps::new();
            assert!(!paths.pool_json().exists());

            let returned =
                run_with_online_marker(Some(&inactive()), Some(&cfg), &paths, &ops, &fs, || {
                    save_membership(&two_disk_membership(), &paths).unwrap();
                    result
                });

            assert_eq!(returned, result);
            assert_eq!(
                ops.calls(),
                vec![
                    "mountpoint".to_owned(),
                    "chown".to_owned(),
                    "chmod".to_owned(),
                    format!("start {BRAID_ONLINE_UNIT}"),
                ]
            );
        }
    }

    // Intent: mark_offline skips braid-online.service when lifecycle is disabled.
    // Why it exists: standalone CLI lock should not spawn systemctl for a
    // module-owned unit that is absent by design.
    // Scenario: CLI-only pool is already unmounted after `braid lock`.
    #[test]
    fn mark_offline_skips_systemctl_when_lifecycle_disabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage"}"#);
        let ops = RecordingOnlineStateOps::new();

        mark_offline(unmounted_proof(), &cfg, &ops);

        assert!(
            !ops.calls()
                .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false"))
        );
    }

    #[test]
    fn mark_offline_stops_when_lifecycle_enabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        mark_offline(unmounted_proof(), &cfg, &ops);
        assert!(
            ops.calls()
                .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false"))
        );
    }

    // Intent: observe_unmounted gives no proof, and warns, when the
    //   mountpoint check itself fails.
    // Why it exists: without the proof, plain lock writes no `done\n` and
    //   does not stop braid-online.service. A stop over an unknown mount
    //   state could leave the unit inactive over a live pool, and the ExecStop
    //   reentry would trust `done\n` and exit 0.
    // Scenario: plain lock finishes, then the mountpoint check returns a
    //   Spawn error mid-shutdown.
    #[test]
    fn observe_unmounted_treats_mountpoint_check_failure_as_mounted() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_mountpoint_err(StagedOnlineFailure::Spawn(
            "mountpoint spawn failure".into(),
        ));
        let mut out = Vec::new();

        let proof = observe_unmounted_to(&cfg, &ops, &mut out);

        assert!(proof.is_none());
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("braid: WARNING: failed to check mountpoint /mnt/storage:"),
            "expected a mountpoint warning, got {out:?}"
        );
    }
}
