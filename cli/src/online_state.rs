//! `braid-online.service` lifecycle fixups that must run under pool lock.
//!
//! These operations used to live in the shell wrapper. Keeping them behind a
//! Rust seam lets dispatch hold `/run/braid-pool.lock` through mount
//! permissions and systemd lifecycle changes.

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::Config;
use crate::types::MountPoint;
use nix::unistd::{Group, User, chown};
#[cfg(test)]
use std::collections::HashMap;
use std::fs;
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
    #[error("systemctl show {unit} failed (exit {exit_code}): {stderr}")]
    SystemctlShow {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    #[error("mountpoint check for {path} failed (exit {exit_code}): {stderr}")]
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
    #[error("systemctl start {unit} failed (exit {exit_code}): {stderr}")]
    SystemctlStart {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
    #[error("systemctl stop {unit} failed (exit {exit_code}): {stderr}")]
    SystemctlStop {
        unit: String,
        exit_code: i32,
        stderr: String,
    },
}

pub trait OnlineStateOps {
    fn unit_active_state(&self, unit: &str) -> Result<UnitActiveState, OnlineError>;
    fn is_mountpoint(&self, path: &Path) -> Result<bool, OnlineError>;
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

    fn is_mountpoint(&self, path: &Path) -> Result<bool, OnlineError> {
        let mount_point = MountPoint::new(path.display().to_string());
        let output = self
            .runner
            .run(&CmdRequest::MountpointCheck { path: mount_point })
            .map_err(|source| OnlineError::Spawn { source })?;
        match output.exit_status {
            0 => Ok(true),
            32 => Ok(false),
            code => Err(OnlineError::Mountpoint {
                path: path.display().to_string(),
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

/// Uses this lock window's entry-state `OnlineSnapshot` to gate `braid-online.service` start.
/// Skipping captured active/activating/deactivating states avoids queueing start behind stop;
/// see the snapshot rule in docs/design/decisions/026-pool-lock-rust-owned.md.
pub fn mark_online(
    snap: Option<&OnlineSnapshot>,
    cfg: &Config,
    ops: &dyn OnlineStateOps,
) -> Result<(), OnlineError> {
    let mount_point = Path::new(cfg.mount_point().as_str());
    let mounted = match ops.is_mountpoint(mount_point) {
        Ok(mounted) => mounted,
        Err(e) => {
            eprintln!("braid: WARNING: failed to check mountpoint {mount_point:?}: {e}");
            return Ok(());
        }
    };
    if !mounted {
        return Ok(());
    }

    if let Some(group) = cfg.pool_access_group() {
        if let Err(e) = ops.chown(mount_point, "root", group) {
            eprintln!(
                "braid: WARNING: failed to set ownership on {}: {e}",
                mount_point.display()
            );
        }
        if let Err(e) = ops.chmod(mount_point, 0o2770) {
            eprintln!(
                "braid: WARNING: failed to set permissions on {}: {e}",
                mount_point.display()
            );
        }
    }

    if cfg.systemd_lifecycle()
        && let Some(snap) = snap
    {
        match &snap.online_state {
            UnitActiveState::Inactive | UnitActiveState::Failed => {
                if ops.systemctl_start(BRAID_ONLINE_UNIT).is_err() {
                    eprintln!(
                        "braid: WARNING: failed to activate braid-online.service -- pool is mounted but shutdown may not lock automatically"
                    );
                }
            }
            UnitActiveState::Unknown(reason) => {
                eprintln!(
                    "braid: WARNING: could not read braid-online.service ActiveState ({reason}) -- pool is mounted but shutdown may not lock automatically"
                );
            }
            UnitActiveState::Active
            | UnitActiveState::Activating
            | UnitActiveState::Deactivating
            | UnitActiveState::Maintenance
            | UnitActiveState::Reloading
            | UnitActiveState::Refreshing => {}
        }
    }

    Ok(())
}

/// Shared online-side finalizer so dispatch cannot skip lifecycle
/// reconciliation after post-mount command failures.
pub fn run_with_online_marker<E>(
    snap: Option<&OnlineSnapshot>,
    cfg: Option<&Config>,
    ops: &dyn OnlineStateOps,
    op: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let result = op();
    if let Some(cfg) = cfg {
        let _ = mark_online(snap, cfg, ops);
    }
    result
}

/// Plain lock finalizer: stop does not use the online snapshot gate.
/// It relies on `/run/braid-stop-coordinator.lock` and the `done\n` protocol from
/// docs/design/decisions/026-pool-lock-rust-owned.md instead.
/// An unknown mountpoint state is treated as still-mounted -- mirrors
/// `mark_online`'s fail-safe and skips the synchronous stop.
pub fn mark_offline(cfg: &Config, ops: &dyn OnlineStateOps) -> Result<(), OnlineError> {
    let path = Path::new(cfg.mount_point().as_str());
    match ops.is_mountpoint(path) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "braid: WARNING: failed to check mountpoint {}: {e}",
                path.display()
            );
            return Ok(());
        }
    }

    if cfg.systemd_lifecycle()
        && let Err(e) = ops.systemctl_stop(BRAID_ONLINE_UNIT, false)
    {
        eprintln!("braid: WARNING: failed to deactivate braid-online.service: {e}");
    }
    Ok(())
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

    fn is_mountpoint(&self, _path: &Path) -> Result<bool, OnlineError> {
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
        Ok(())
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
                path: MountPoint::new("/mnt/storage".into()),
            },
            mountpoint_out(0, ""),
        );
        let ops = RealOnlineStateOps::new(&runner);

        assert!(ops.is_mountpoint(Path::new("/mnt/storage")).unwrap());
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
                path: MountPoint::new("/mnt/storage".into()),
            },
            mountpoint_out(32, "/mnt/storage is not a mountpoint\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        assert!(!ops.is_mountpoint(Path::new("/mnt/storage")).unwrap());
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
                path: MountPoint::new("/mnt/storage".into()),
            },
            mountpoint_out(1, "bad usage\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        let err = ops
            .is_mountpoint(Path::new("/mnt/storage"))
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
                path: MountPoint::new("/mnt/storage".into()),
            },
            mountpoint_out(2, "unexpected failure\n"),
        );
        let ops = RealOnlineStateOps::new(&runner);

        let err = ops
            .is_mountpoint(Path::new("/mnt/storage"))
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

    // Intent: mark_online skips braid-online.service when lifecycle is disabled.
    // Why it exists: standalone CLI configs still run mount fixups but do not
    // own the module-defined online unit.
    // Scenario: `braid add` succeeds in a CLI-only test VM with no
    // braid-online.service installed.
    #[test]
    fn mark_online_skips_systemctl_when_lifecycle_disabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage"}"#);
        let ops = RecordingOnlineStateOps::new();

        mark_online(
            Some(&OnlineSnapshot {
                online_state: UnitActiveState::Inactive,
            }),
            &cfg,
            &ops,
        )
        .unwrap();

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

        mark_online(
            Some(&OnlineSnapshot {
                online_state: UnitActiveState::Inactive,
            }),
            &cfg,
            &ops,
        )
        .unwrap();

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
            mark_online(
                Some(&OnlineSnapshot {
                    online_state: state,
                }),
                &cfg,
                &ops,
            )
            .unwrap();
            assert!(ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
        }

        let ops = RecordingOnlineStateOps::new();
        mark_online(
            Some(&OnlineSnapshot {
                online_state: UnitActiveState::Deactivating,
            }),
            &cfg,
            &ops,
        )
        .unwrap();
        assert!(!ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));

        let ops = RecordingOnlineStateOps::new();
        mark_online(
            Some(&OnlineSnapshot {
                online_state: UnitActiveState::Maintenance,
            }),
            &cfg,
            &ops,
        )
        .unwrap();
        assert!(!ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
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

        mark_online(None, &cfg, &ops).unwrap();

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
        let snap = OnlineSnapshot {
            online_state: UnitActiveState::Inactive,
        };

        let result = run_with_online_marker(Some(&snap), Some(&cfg), &ops, || {
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
        let snap = OnlineSnapshot {
            online_state: UnitActiveState::Inactive,
        };

        let result =
            run_with_online_marker(Some(&snap), Some(&cfg), &ops, || Ok::<(), &'static str>(()));

        assert_eq!(result, Ok(()));
        assert!(ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: the online finalizer relies on mark_online's mountpoint gate on
    // failure paths.
    // Why it exists: pre-mount command errors must not activate
    // braid-online.service for an offline pool.
    // Scenario: planning or credential verification fails before anything is
    // mounted, then dispatch still runs the finalizer.
    #[test]
    fn run_with_online_marker_skips_when_mountpoint_false() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);
        let snap = OnlineSnapshot {
            online_state: UnitActiveState::Inactive,
        };

        let result =
            run_with_online_marker(Some(&snap), Some(&cfg), &ops, || Err::<(), _>("pre-mount"));

        assert_eq!(result, Err("pre-mount"));
        assert!(!ops.calls().contains(&format!("start {BRAID_ONLINE_UNIT}")));
    }

    // Intent: dry-run dispatch bypasses online lifecycle reconciliation.
    // Why it exists: dry-run commands may share command plumbing but must not
    // probe mountpoints or touch systemd state.
    // Scenario: dispatch passes no config to the finalizer for a dry-run
    // command that returns an error.
    #[test]
    fn run_with_online_marker_skips_when_config_none() {
        let ops = RecordingOnlineStateOps::new();
        let snap = OnlineSnapshot {
            online_state: UnitActiveState::Inactive,
        };

        let result =
            run_with_online_marker(Some(&snap), None, &ops, || Err::<(), _>("dry-run failure"));

        assert_eq!(result, Err("dry-run failure"));
        assert!(ops.calls().is_empty());
    }

    // Intent: mark_offline skips braid-online.service when lifecycle is disabled.
    // Why it exists: standalone CLI lock should not spawn systemctl for a
    // module-owned unit that is absent by design.
    // Scenario: CLI-only pool is already unmounted after `braid lock`.
    #[test]
    fn mark_offline_skips_systemctl_when_lifecycle_disabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage"}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);

        mark_offline(&cfg, &ops).unwrap();

        assert!(
            !ops.calls()
                .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false"))
        );
    }

    #[test]
    fn mark_offline_stops_when_lifecycle_enabled() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);
        mark_offline(&cfg, &ops).unwrap();
        assert!(
            ops.calls()
                .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false"))
        );
    }

    // Intent: mark_offline must not stop braid-online.service when the
    // mountpoint check itself fails.
    // Why it exists: cmd_lock_orchestrate writes `done\n` to the stop
    // coordinator before mark_offline; ExecStop reentry would treat that
    // marker as authoritative and exit 0, so a stop transition over an
    // unknown mount state could leave the unit inactive over a live pool.
    // Scenario: cmd_lock succeeded and the done marker is set, then the
    // mountpoint check returns a Spawn error mid-shutdown.
    #[test]
    fn mark_offline_skips_systemctl_when_mountpoint_check_fails() {
        let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
        let ops = RecordingOnlineStateOps::new();
        ops.set_mountpoint_err(StagedOnlineFailure::Spawn(
            "mountpoint spawn failure".into(),
        ));

        mark_offline(&cfg, &ops).unwrap();

        assert!(
            !ops.calls()
                .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false")),
            "expected no systemctl stop after mountpoint check failure, got {:?}",
            ops.calls(),
        );
    }
}
