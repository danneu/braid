//! `braid-online.service` lifecycle fixups that must run under pool lock.
//!
//! These operations used to live in the shell wrapper. Keeping them behind a
//! Rust seam lets dispatch hold `/run/braid-pool.lock` through mount
//! permissions and systemd lifecycle changes.

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::Config;
use crate::types::MountPoint;
use nix::unistd::{Group, User, chown};
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
            "reloading" => Self::Reloading,
            "refreshing" => Self::Refreshing,
            other => Self::Unknown(other.to_owned()),
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
        let mount_point = MountPoint(path.display().to_string());
        let output = self
            .runner
            .run(&CmdRequest::MountpointCheck { path: mount_point })
            .map_err(|source| OnlineError::Spawn { source })?;
        match output.exit_status {
            0 => Ok(true),
            1 => Ok(false),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineSnapshot {
    pub online_state: UnitActiveState,
}

pub fn snapshot(ops: &dyn OnlineStateOps) -> OnlineSnapshot {
    let online_state = ops
        .unit_active_state(BRAID_ONLINE_UNIT)
        .unwrap_or_else(|e| UnitActiveState::Unknown(e.to_string()));
    OnlineSnapshot { online_state }
}

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
            | UnitActiveState::Reloading
            | UnitActiveState::Refreshing => {}
        }
    }

    Ok(())
}

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
pub struct RecordingOnlineStateOps {
    state: std::cell::RefCell<Result<UnitActiveState, String>>,
    mounted: std::cell::Cell<bool>,
    calls: std::cell::RefCell<Vec<String>>,
    bound_by: std::cell::RefCell<Result<Vec<String>, String>>,
}

#[cfg(test)]
impl RecordingOnlineStateOps {
    pub fn new() -> Self {
        Self {
            state: std::cell::RefCell::new(Ok(UnitActiveState::Inactive)),
            mounted: std::cell::Cell::new(true),
            calls: std::cell::RefCell::new(Vec::new()),
            bound_by: std::cell::RefCell::new(Ok(Vec::new())),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    pub fn set_state(&self, state: UnitActiveState) {
        *self.state.borrow_mut() = Ok(state);
    }

    pub fn set_mounted(&self, mounted: bool) {
        self.mounted.set(mounted);
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
        Ok(self.mounted.get())
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
        Ok(())
    }

    fn list_bound_by(&self, _unit: &str) -> Result<Vec<String>, OnlineError> {
        self.bound_by
            .borrow()
            .clone()
            .map_err(|s| OnlineError::Spawn {
                source: CmdError::Failed(s),
            })
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
}
