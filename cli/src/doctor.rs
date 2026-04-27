use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Path written by `modules/braid/monitor.nix` (`environment.etc."braid/notifier-config.json"`).
/// `check_beep_path` reads it to discover the canonical beep wrapper.
const NOTIFIER_CONFIG_PATH: &str = "/etc/braid/notifier-config.json";

/// Schema of `/etc/braid/notifier-config.json`. Tracked in lockstep with the
/// `builtins.toJSON` writer in `modules/braid/monitor.nix`. A schema change
/// must update both sides — deserialize errors here are loud (Fail), so a
/// stale parser cannot silently degrade.
#[derive(Debug, Clone, Deserialize)]
struct NotifierConfig {
    beep_probe_path: Option<String>,
}

use crate::cmd::{CmdRequest, CommandRunner, RealRunner};
use crate::config::Config;
use crate::luks;
use crate::membership;
use crate::parse::parse_btrfs_df_json;
use crate::parse::types::{BtrfsBgType, BtrfsDfOutput, BtrfsProfile};
use crate::preflight;
use crate::state_paths::StatePaths;
use crate::status::format_bytes;
use crate::status_tag::{StatusTag, color_enabled_for_stdout, status_line};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub status: CheckStatus,
    pub checks: Vec<CheckResult>,
}

enum DfSnapshot {
    NotMounted,
    QueryFailed(String),
    ParseFailed(String),
    Ok(BtrfsDfOutput),
}

struct DoctorContext<'a, R: CommandRunner> {
    config_path: PathBuf,
    config_value: Option<serde_json::Value>,
    config: Option<Config>,
    runner: &'a R,
    paths: &'a StatePaths,
    mountpoint_is_mounted: Option<bool>,
    df_snapshot: Option<DfSnapshot>,
}

#[derive(Debug, Clone, Copy)]
pub struct DoctorOptions {
    pub json: bool,
    pub beep: bool,
}

#[derive(Debug, Clone, Copy)]
struct BeepCheckOptions {
    is_root: bool,
    json_output: bool,
    play_beep: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraidOnlineActiveState {
    OkSettled,
    Activating,
    Fail,
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn check_config_file<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let path = &ctx.config_path;

    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Fail,
                message: format!("{}: {e}", path.display()),
            };
        }
    };

    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => {
            ctx.config_value = Some(v);
            CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Ok,
                message: format!("{} exists and is valid JSON", path.display()),
            }
        }
        Err(e) => CheckResult {
            name: "config_file".into(),
            status: CheckStatus::Fail,
            message: format!("{}: invalid JSON: {e}", path.display()),
        },
    }
}

fn check_config_schema<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let value = match &ctx.config_value {
        Some(v) => v.clone(),
        None => {
            return CheckResult {
                name: "config_schema".into(),
                status: CheckStatus::Skip,
                message: "skipped (config file not available)".into(),
            };
        }
    };

    let cfg: Config = match serde_json::from_value(value) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name: "config_schema".into(),
                status: CheckStatus::Fail,
                message: format!("failed to deserialize config: {e}"),
            };
        }
    };

    ctx.config = Some(cfg);
    CheckResult {
        name: "config_schema".into(),
        status: CheckStatus::Ok,
        message: "required fields present and valid".into(),
    }
}

fn check_config_permissions<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config_value.is_none() {
        return CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Skip,
            message: "skipped (config file not available)".into(),
        };
    }

    let meta = match std::fs::metadata(&ctx.config_path) {
        Ok(m) => m,
        Err(e) => {
            return CheckResult {
                name: "config_permissions".into(),
                status: CheckStatus::Warn,
                message: format!("could not stat {}: {e}", ctx.config_path.display()),
            };
        }
    };

    let mode = meta.mode();
    let uid = meta.uid();
    let mut warnings: Vec<String> = Vec::new();

    // World-writable (o+w)
    if mode & 0o002 != 0 {
        warnings.push("world-writable".into());
    }
    // Group-writable (g+w)
    if mode & 0o020 != 0 {
        warnings.push("group-writable".into());
    }
    // Owner is not root
    if uid != 0 {
        warnings.push(format!("owned by uid {uid}, expected root (0)"));
    }

    if warnings.is_empty() {
        CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Ok,
            message: format!("{} permissions ok", ctx.config_path.display()),
        }
    } else {
        CheckResult {
            name: "config_permissions".into(),
            status: CheckStatus::Warn,
            message: format!("{}: {}", ctx.config_path.display(), warnings.join(", ")),
        }
    }
}

/// Per-disk health classification used by `check_declared_disks`.
///
/// The `Luks*` variants describe what cryptsetup probes saw on disk; the rest
/// describe earlier failure modes (filesystem-level or runner-level) where we
/// never reached a probe. Keeping them in one enum lets `summarize_declared_disks`
/// produce a single aggregated finding per disk.
#[derive(Debug, Clone)]
enum DiskState {
    /// Both `cryptsetup isLuks` and `cryptsetup luksDump` succeeded.
    LuksHeaderOk,
    /// `std::fs::metadata` returned `Err` — the by-id symlink target is gone.
    Missing,
    /// Path exists but is not a block device.
    NotBlock,
    /// The cryptsetup command itself failed to execute (missing binary, IPC
    /// failure, etc.). NOT the same as cryptsetup inspecting the device and
    /// finding it damaged — this is a tooling problem and must never produce
    /// a "repair the LUKS header" suggestion.
    ProbeFailed(String),
    /// `cryptsetup isLuks` exited non-zero — the LUKS magic is gone or the
    /// header is otherwise unreadable. Severe.
    LuksHeaderUnreadable,
    /// `cryptsetup isLuks` succeeded but `cryptsetup luksDump` failed —
    /// the magic is intact but metadata is damaged. Less severe;
    /// `cryptsetup repair --type luks2` may be able to fix it.
    LuksHeaderDamaged,
}

/// Probe a single declared disk to figure out its `DiskState`.
///
/// This is the impure half of `check_declared_disks`: it touches the filesystem
/// (`std::fs::metadata`) and the runner. It is intentionally tiny so the only
/// untested code path is the unavoidable filesystem gate; the rendering logic
/// lives in `summarize_declared_disks`, which is pure and unit-tested.
///
/// The LUKS-specific probe sequence is delegated to `luks::probe_luks_header`
/// so that `doctor` and `unlock` share the same classification (and the same
/// remediation message strings downstream).
fn classify_disk_state<R: CommandRunner>(runner: &R, path: &Path) -> DiskState {
    match std::fs::metadata(path) {
        Err(_) => return DiskState::Missing,
        Ok(meta) if !meta.file_type().is_block_device() => return DiskState::NotBlock,
        Ok(_) => {}
    }

    let device = path.to_string_lossy().into_owned();
    match luks::probe_luks_header(runner, &device) {
        luks::LuksHeaderState::Ok => DiskState::LuksHeaderOk,
        luks::LuksHeaderState::Unreadable => DiskState::LuksHeaderUnreadable,
        luks::LuksHeaderState::Damaged => DiskState::LuksHeaderDamaged,
        luks::LuksHeaderState::ProbeFailed(err) => DiskState::ProbeFailed(err),
    }
}

/// Pure rendering function: takes pre-classified per-disk states and returns
/// the `CheckResult` for `declared_disks`.
///
/// Remediation messages delegate to `luks::luks_header_unreadable_guidance`
/// and `luks::luks_header_damaged_guidance`, which are shared with the unlock
/// error-enrichment path. Those helpers enforce the cross-command invariant
/// that no user-facing message ever references local
/// `/var/lib/braid/luks-headers/` files — `braid status` and the TUI already
/// warn about persistent local copies, because the intended workflow is to
/// export headers off-system and remove the local copy.
fn summarize_declared_disks(classifications: &[(String, String, DiskState)]) -> CheckResult {
    let total = classifications.len();
    let mut missing: Vec<String> = Vec::new();
    let mut not_block: Vec<String> = Vec::new();
    let mut probe_failed: Vec<String> = Vec::new();
    let mut header_unreadable: Vec<String> = Vec::new();
    let mut header_damaged: Vec<String> = Vec::new();

    for (name, by_id, state) in classifications {
        match state {
            DiskState::LuksHeaderOk => {}
            DiskState::Missing => missing.push(format!("{name} ({by_id})")),
            DiskState::NotBlock => not_block.push(format!("{name} ({by_id})")),
            DiskState::ProbeFailed(err) => {
                probe_failed.push(format!("{name} ({by_id}): {err}"));
            }
            DiskState::LuksHeaderUnreadable => {
                header_unreadable.push(format!(
                    "{name} ({by_id}): {}",
                    luks::luks_header_unreadable_guidance()
                ));
            }
            DiskState::LuksHeaderDamaged => {
                header_damaged.push(format!(
                    "{name} ({by_id}): {}",
                    luks::luks_header_damaged_guidance(by_id)
                ));
            }
        }
    }

    let problem_count = missing.len()
        + not_block.len()
        + probe_failed.len()
        + header_unreadable.len()
        + header_damaged.len();

    if problem_count == 0 {
        return CheckResult {
            name: "declared_disks".into(),
            status: CheckStatus::Ok,
            message: format!("all {total} declared disk(s) present"),
        };
    }

    let mut parts: Vec<String> = Vec::new();
    if !missing.is_empty() {
        parts.push(format!(
            "{} not found: {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    if !not_block.is_empty() {
        parts.push(format!(
            "{} not a block device: {}",
            not_block.len(),
            not_block.join(", ")
        ));
    }
    if !header_unreadable.is_empty() {
        parts.push(format!(
            "{} with unreadable LUKS header: {}",
            header_unreadable.len(),
            header_unreadable.join("; ")
        ));
    }
    if !header_damaged.is_empty() {
        parts.push(format!(
            "{} with damaged LUKS header metadata: {}",
            header_damaged.len(),
            header_damaged.join("; ")
        ));
    }
    if !probe_failed.is_empty() {
        parts.push(format!(
            "{} with LUKS header probe failures: {}",
            probe_failed.len(),
            probe_failed.join("; ")
        ));
    }

    CheckResult {
        name: "declared_disks".into(),
        status: CheckStatus::Warn,
        message: format!(
            "{}/{} disk(s) have problems: {}",
            problem_count,
            total,
            parts.join("; ")
        ),
    }
}

fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let pool_membership = match membership::load_membership(ctx.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::NotFound(_)) => {
            return CheckResult {
                name: "declared_disks".into(),
                status: CheckStatus::Skip,
                message: "skipped (no pool membership file)".into(),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "declared_disks".into(),
                status: CheckStatus::Warn,
                message: format!("could not load pool membership: {e}"),
            };
        }
    };

    let classifications: Vec<(String, String, DiskState)> = pool_membership
        .disks
        .iter()
        .map(|(name, member)| {
            let by_id = member.by_id.0.clone();
            let state = classify_disk_state(ctx.runner, Path::new(&by_id));
            (name.clone(), by_id, state)
        })
        .collect();

    summarize_declared_disks(&classifications)
}

fn ensure_mountpoint_is_mounted<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> Option<bool> {
    let config = match &ctx.config {
        Some(c) => c,
        None => return None,
    };

    if let Some(is_mounted) = ctx.mountpoint_is_mounted {
        return Some(is_mounted);
    }

    let mount_point = config.mount_point().to_owned();

    let is_mounted = matches!(
        ctx.runner.run(&CmdRequest::MountpointCheck {
            path: mount_point,
        }),
        Ok(out) if out.exit_status == 0
    );
    ctx.mountpoint_is_mounted = Some(is_mounted);
    Some(is_mounted)
}

fn ensure_df_snapshot<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) {
    if ctx.df_snapshot.is_some() {
        return;
    }

    let config = match &ctx.config {
        Some(c) => c,
        None => return,
    };

    let mount_point = config.mount_point().to_owned();

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        ctx.df_snapshot = Some(DfSnapshot::NotMounted);
        return;
    }

    // Query btrfs filesystem df
    let raw = match ctx
        .runner
        .run(&CmdRequest::BtrfsFilesystemDfJson { mount_point })
    {
        Ok(raw) => raw,
        Err(e) => {
            ctx.df_snapshot = Some(DfSnapshot::QueryFailed(e.to_string()));
            return;
        }
    };

    match parse_btrfs_df_json(&raw) {
        Ok(df) => ctx.df_snapshot = Some(DfSnapshot::Ok(df)),
        Err(e) => ctx.df_snapshot = Some(DfSnapshot::ParseFailed(e.to_string())),
    }
}

fn check_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    bg_type: BtrfsBgType,
    check_name: &str,
    type_label: &str,
) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    }

    ensure_df_snapshot(ctx);

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();

    match &ctx.df_snapshot {
        None => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        },
        Some(DfSnapshot::NotMounted) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Skip,
            message: "skipped (pool not mounted)".into(),
        },
        Some(DfSnapshot::QueryFailed(e)) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Warn,
            message: format!("could not query {type_label} profiles: {e}"),
        },
        Some(DfSnapshot::ParseFailed(e)) => CheckResult {
            name: check_name.into(),
            status: CheckStatus::Warn,
            message: format!("could not parse {type_label} profiles: {e}"),
        },
        Some(DfSnapshot::Ok(df)) => {
            let entries: Vec<_> = df.entries.iter().filter(|e| e.bg_type == bg_type).collect();

            let profiles: std::collections::BTreeSet<&BtrfsProfile> =
                entries.iter().map(|e| &e.bg_profile).collect();

            if profiles.len() <= 1 {
                let profile_name = profiles
                    .into_iter()
                    .next()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into());
                CheckResult {
                    name: check_name.into(),
                    status: CheckStatus::Ok,
                    message: format!("{type_label} profile: {profile_name}"),
                }
            } else {
                let mut parts: Vec<String> = Vec::new();
                for entry in &entries {
                    parts.push(format!(
                        "{}: {} used / {} total",
                        entry.bg_profile,
                        format_bytes(entry.bg_used),
                        format_bytes(entry.bg_total),
                    ));
                }
                CheckResult {
                    name: check_name.into(),
                    status: CheckStatus::Warn,
                    message: format!(
                        "mixed {type_label} profiles ({}); run: btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft {mount_point}",
                        parts.join(", "),
                    ),
                }
            }
        }
    }
}

fn check_pool_missing_devices<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Skip,
            message: "skipped (pool not mounted)".into(),
        };
    }

    let mount_point = ctx.config.as_ref().unwrap().mount_point().clone();

    match preflight::probe_missing_devids(ctx.runner, &mount_point) {
        Ok(missing) if missing.is_empty() => CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Ok,
            message: "no missing devices".into(),
        },
        Ok(missing) => {
            let devids: Vec<String> = missing.iter().map(|d| d.to_string()).collect();
            CheckResult {
                name: "pool_missing_devices".into(),
                status: CheckStatus::Warn,
                message: format!(
                    "pool has {} missing device{} (devid{}: {}); replace with: braid replace --old <disk> --new <disk> --missing-id <devid>",
                    missing.len(),
                    if missing.len() == 1 { "" } else { "s" },
                    if missing.len() == 1 { "" } else { "s" },
                    devids.join(", "),
                ),
            }
        }
        Err(e) => CheckResult {
            name: "pool_missing_devices".into(),
            status: CheckStatus::Warn,
            message: format!("could not probe for missing devices: {e}"),
        },
    }
}

fn check_data_profile_mismatch<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    check_profile_mismatch(ctx, BtrfsBgType::Data, "data_profile_mismatch", "data")
}

fn check_metadata_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    check_profile_mismatch(
        ctx,
        BtrfsBgType::Metadata,
        "metadata_profile_mismatch",
        "metadata",
    )
}

/// Doctor check for the PC speaker alert path.
///
/// By default, validates the notifier config without playing sound. Passing
/// `--beep` plays a short alert test beep (1 kHz, 500 ms) via the canonical
/// `braid-beep-probe` wrapper -- the same code path the alert service uses.
/// A successful `--beep` run is both a notifier-health check and a positive
/// guarantee that future disk alerts will produce the same audible beep.
///
/// `--json` mode (`json_output = true`) suppresses the beep: machine-readable
/// output must never produce audible side effects. The check still appears in
/// the JSON report (as `Skip`) so scripts auditing doctor output can see it.
///
/// This is the public entry point. It hits the real filesystem and the
/// real `geteuid()` syscall; unit tests target `check_beep_path_inner`
/// directly so the geteuid and json branches are exercised deterministically
/// regardless of which UID `cargo test` runs under.
fn check_beep_path<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    options: DoctorOptions,
) -> CheckResult {
    let is_root = unsafe { libc::geteuid() } == 0;
    check_beep_path_inner(
        ctx,
        Path::new(NOTIFIER_CONFIG_PATH),
        BeepCheckOptions {
            is_root,
            json_output: options.json,
            play_beep: options.beep,
        },
    )
}

/// UPS doctor check: warn when `braid.ups.enable = true` but the
/// `upsc` probe fails.
///
/// Severity is `Warn`, not `Fail`: the operator can fix daemon state
/// directly (e.g. `systemctl start upsd`); braid does not intervene in
/// NUT lifecycle. Skips with a distinct reason when config is unavailable;
/// otherwise skips when UPS is not configured or disabled.
fn check_ups_daemon_up<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let name = "ups_daemon".to_string();
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    };
    let ups_cfg = match config.ups() {
        Some(u) if u.enable => u,
        _ => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (braid.ups not enabled)".into(),
            };
        }
    };
    let raw = match ctx.runner.run(&CmdRequest::UpscQuery {
        name: ups_cfg.name.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                name,
                status: CheckStatus::Warn,
                message: format!("upsc failed to spawn: {e} -- is pkgs.nut on PATH?"),
            };
        }
    };
    match crate::parse::parse_upsc(&raw) {
        Ok(out) => {
            if out.status_flags.is_empty() {
                CheckResult {
                    name,
                    status: CheckStatus::Warn,
                    message: format!(
                        "upsc {} responded but ups.status is empty -- driver may still be starting",
                        ups_cfg.name
                    ),
                }
            } else {
                CheckResult {
                    name,
                    status: CheckStatus::Ok,
                    message: format!("upsc {} reachable", ups_cfg.name),
                }
            }
        }
        Err(_) => CheckResult {
            name,
            status: CheckStatus::Warn,
            message: format!(
                "upsc {} unreachable -- check 'systemctl status upsd.service'",
                ups_cfg.name
            ),
        },
    }
}

fn classify_braid_online_active_state(state: &str) -> BraidOnlineActiveState {
    match state {
        "active" | "reloading" | "refreshing" => BraidOnlineActiveState::OkSettled,
        "activating" => BraidOnlineActiveState::Activating,
        _ => BraidOnlineActiveState::Fail,
    }
}

fn read_braid_online_active_state<R: CommandRunner>(
    runner: &R,
) -> Result<String, crate::cmd::CmdError> {
    let raw = runner.run(&CmdRequest::SystemctlIsActive {
        unit: "braid-online.service".into(),
    })?;
    Ok(raw.stdout.trim().to_owned())
}

/// UPS doctor check: report braid-online.service state while the pool is
/// mounted under UPS.
///
/// This is the critical configuration fault in `docs/decisions/020-
/// ups-integration.md`'s "braid-online becomes safety-critical"
/// section: without `braid-online.service` active, reloading, or
/// refreshing, the `SHUTDOWNCMD = systemctl poweroff` path does NOT
/// unwind `braid lock`'s ExecStop. `activating` is only a Warn because it is
/// plausibly transient, but every other non-success state is a high-severity
/// fault.
///
/// Skips with a distinct reason when config is unavailable. Otherwise skips
/// when UPS is disabled OR when the pool is not mounted (no safety implication
/// then).
fn check_braid_online_active_when_mounted<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    let fail_result = |name: String, state: &str| CheckResult {
        name,
        status: CheckStatus::Fail,
        message: format!(
            "braid-online.service is {state} -- UPS shutdown will not unmount the pool. \
             Run `systemctl start braid-online.service` or re-run `braid unlock`."
        ),
    };
    let warn_result = |name: String| {
        CheckResult {
            name,
            status: CheckStatus::Warn,
            message: "braid-online.service is activating -- UPS shutdown hook is not confirmed yet; re-run braid doctor shortly"
                .into(),
        }
    };

    let name = "braid_online_active".to_string();
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (config not available)".into(),
        };
    };
    if !config.ups().is_some_and(|u| u.enable) {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (braid.ups not enabled)".into(),
        };
    }
    let mount_point = config.mount_point().clone();
    match ctx.runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    }) {
        Ok(out) if out.exit_status == 0 => {}
        _ => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (pool not mounted -- braid-online only matters while online)"
                    .into(),
            };
        }
    }
    let state = match read_braid_online_active_state(ctx.runner) {
        Ok(state) => state,
        Err(e) => {
            return CheckResult {
                name,
                status: CheckStatus::Fail,
                message: format!("systemctl spawn failed: {e}"),
            };
        }
    };
    match classify_braid_online_active_state(&state) {
        BraidOnlineActiveState::OkSettled => CheckResult {
            name,
            status: CheckStatus::Ok,
            message: format!("braid-online.service is {state}"),
        },
        BraidOnlineActiveState::Activating => warn_result(name),
        BraidOnlineActiveState::Fail => fail_result(name, &state),
    }
}

fn check_beep_path_inner<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    notifier_path: &Path,
    options: BeepCheckOptions,
) -> CheckResult {
    let name = "beep_path".to_string();

    // 1. Read the notifier config the NixOS module wrote. Absent -> Skip.
    //    A bare `braid` install (no monitor module imported) won't have it.
    let raw = match std::fs::read_to_string(notifier_path) {
        Ok(s) => s,
        Err(_) => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (braid monitor not configured)".into(),
            };
        }
    };

    // 2. Parse. Malformed = real defect: the module wrote junk.
    let cfg: NotifierConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name,
                status: CheckStatus::Fail,
                message: format!("{}: malformed: {e}", notifier_path.display()),
            };
        }
    };

    // 3. Beep disabled is a clean Skip.
    let probe_path = match cfg.beep_probe_path {
        Some(p) => p,
        None => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (beep monitoring disabled)".into(),
            };
        }
    };

    // 4. Lack of root is an INVOCATION CONTEXT issue, not a SPEAKER HEALTH
    //    issue. The wrapper does setpriv --reuid=nobody, which requires
    //    CAP_SETUID. Reporting Fail here would make doctor untrustworthy:
    //    "speaker is broken" and "you ran doctor without sudo" are
    //    different conditions. Checked BEFORE the JSON gate so non-root
    //    callers always get the actionable "use sudo" hint regardless of
    //    output mode. The is_root flag is computed by the public wrapper
    //    above so unit tests can deterministically exercise both branches.
    if !options.is_root {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (requires root to play the alert test beep)".into(),
        };
    }

    // 5. JSON mode is for programmatic consumption -- emitting an audible
    //    side effect from a data-output command is wrong. The check still
    //    appears in the report (as Skip) so scripts auditing doctor output
    //    can see it; the wrapper is simply never invoked.
    if options.json_output {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped in --json mode -- rerun with --beep without --json to play the alert test beep"
                .into(),
        };
    }

    // 6. Plain doctor confirms beep monitoring is configured without playing
    //    sound. The runner is only invoked for explicit --beep.
    if !options.play_beep {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (pass --beep to play the audible alert test beep)".into(),
        };
    }

    // 7. Run the canonical wrapper. This PLAYS the real short alert beep
    //    (1 kHz, 500 ms) -- same code path the alert service uses. Hearing
    //    the beep is both the success signal AND a preview of what real
    //    disk alerts will sound like.
    match ctx
        .runner
        .run(&CmdRequest::BraidBeepProbe { path: probe_path })
    {
        Ok(out) if out.exit_status == 0 => CheckResult {
            name,
            status: CheckStatus::Ok,
            message: "alert test beep command succeeded -- you should have heard a 1 kHz, 500 ms disk-alert beep"
                .into(),
        },
        Ok(out) => CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!(
                "could not play alert test beep (braid-beep-probe exited {}) \
                 -- speaker likely broken: missing pcspkr device, evdev \
                 permissions wrong, or kmod blacklist still active: {}",
                out.exit_status,
                out.stderr.trim()
            ),
        },
        Err(e) => CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!(
                "could not play alert test beep (braid-beep-probe failed to spawn): {e}"
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn overall_status(checks: &[CheckResult]) -> CheckStatus {
    let mut worst = CheckStatus::Ok;
    for c in checks {
        worst = match (worst, c.status) {
            (_, CheckStatus::Skip) => worst,
            (CheckStatus::Fail, _) => CheckStatus::Fail,
            (_, CheckStatus::Fail) => CheckStatus::Fail,
            (CheckStatus::Warn, _) => CheckStatus::Warn,
            (_, CheckStatus::Warn) => CheckStatus::Warn,
            _ => worst,
        };
    }
    worst
}

pub fn run_doctor<R: CommandRunner>(
    config_path: &Path,
    runner: &R,
    paths: &StatePaths,
    options: DoctorOptions,
) -> DoctorReport {
    let mut ctx = DoctorContext {
        config_path: config_path.to_owned(),
        config_value: None,
        config: None,
        runner,
        paths,
        mountpoint_is_mounted: None,
        df_snapshot: None,
    };

    let checks = vec![
        check_config_file(&mut ctx),
        check_config_schema(&mut ctx),
        check_config_permissions(&mut ctx),
        check_declared_disks(&mut ctx),
        check_pool_missing_devices(&mut ctx),
        check_data_profile_mismatch(&mut ctx),
        check_metadata_profile_mismatch(&mut ctx),
        check_beep_path(&mut ctx, options),
        check_ups_daemon_up(&mut ctx),
        check_braid_online_active_when_mounted(&mut ctx),
    ];

    let status = overall_status(&checks);

    DoctorReport { status, checks }
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

pub fn format_doctor_human(report: &DoctorReport) -> String {
    format_doctor_human_with(report, false)
}

pub fn format_doctor_human_with(report: &DoctorReport, color_enabled: bool) -> String {
    let mut out = String::new();
    for c in &report.checks {
        let tag = match c.status {
            CheckStatus::Ok => StatusTag::Ok,
            CheckStatus::Warn => StatusTag::Warn,
            CheckStatus::Fail => StatusTag::Fail,
            CheckStatus::Skip => StatusTag::Skip,
        };
        let label = match c.name.as_str() {
            "config_file" => "config file",
            "config_schema" => "config schema",
            "config_permissions" => "config perms",
            "declared_disks" => "declared disks",
            "pool_missing_devices" => "missing devs",
            "data_profile_mismatch" => "data profiles",
            "metadata_profile_mismatch" => "meta profiles",
            // The internal identifier `beep_path` stays stable for the JSON
            // schema; the human label reflects the product framing — what
            // the operator hears, not what the code does.
            "beep_path" => "alert beep",
            "ups_daemon" => "ups daemon",
            "braid_online_active" => "braid-online",
            other => other,
        };
        out.push_str(&status_line(
            tag,
            color_enabled,
            &format!("{label:<14}  {}", c.message),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Command entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DoctorError {
    #[error("doctor found failures")]
    Failed,
    #[error("failed to serialize doctor report: {0}")]
    Serialize(#[source] serde_json::Error),
}

pub fn cmd_doctor(
    config_path: &Path,
    paths: &StatePaths,
    options: DoctorOptions,
) -> Result<(), DoctorError> {
    let runner = RealRunner;
    let report = run_doctor(config_path, &runner, paths, options);

    if options.json {
        // serde_json::to_string_pretty won't fail on our types
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(DoctorError::Serialize)?
        );
    } else {
        print!(
            "{}",
            format_doctor_human_with(&report, color_enabled_for_stdout())
        );
    }

    match report.status {
        CheckStatus::Fail => Err(DoctorError::Failed),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;
    use crate::types::MountPoint;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::{NamedTempFile, TempDir};

    fn isolated_paths() -> (TempDir, StatePaths) {
        let dir = TempDir::new().unwrap();
        let paths = StatePaths::custom(dir.path().to_owned());
        (dir, paths)
    }

    fn valid_config_json() -> &'static str {
        r#"{"disks":{"toshiba":{"by_id":"/dev/disk/by-id/a"}},"mount_point":"/mnt/storage"}"#
    }

    fn mock() -> MockRunner {
        MockRunner::default()
    }

    fn human_options() -> DoctorOptions {
        DoctorOptions {
            json: false,
            beep: false,
        }
    }

    fn beep_check_options(is_root: bool, json_output: bool, play_beep: bool) -> BeepCheckOptions {
        BeepCheckOptions {
            is_root,
            json_output,
            play_beep,
        }
    }

    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn find_check<'a>(report: &'a DoctorReport, name: &str) -> &'a CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    #[test]
    fn valid_config_parses_ok_disks_warn() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths, human_options());
        assert_eq!(report.checks.len(), 10);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        // declared_disks skips since no pool membership file exists in test env
        assert_eq!(
            find_check(&report, "declared_disks").status,
            CheckStatus::Skip
        );
        // beep_path is intentionally not asserted here: it depends on real
        // host state (/etc/braid/notifier-config.json and geteuid()).
        // Deterministic coverage lives in the check_beep_path_inner tests.
    }

    #[test]
    fn missing_file_fail_skip() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            human_options(),
        );
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Fail);
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Skip
        );
        assert_eq!(
            find_check(&report, "config_permissions").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn invalid_json_fail_skip() {
        let f = write_temp("not json at all {{{");
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        assert_eq!(report.status, CheckStatus::Fail);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Fail);
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Skip
        );
        assert_eq!(
            find_check(&report, "config_permissions").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn valid_json_with_extra_fields_parses_ok() {
        // Config no longer has disks — extra fields are ignored.
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
    }

    #[test]
    fn valid_json_bad_schema_empty_mount() {
        let f = write_temp(r#"{"disks":{"a":{"by_id":"/dev/disk/by-id/a"}},"mount_point":""}"#);
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        let schema = find_check(&report, "config_schema");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(
            schema.message.contains("mount_point must not be empty"),
            "unexpected message: {}",
            schema.message
        );
    }

    /* Intent: run_doctor distinguishes schema-invalid config from UPS disabled.
     * Why it exists: `check_config_schema` only populates ctx.config after full
     * deserialization succeeds; later UPS checks must report that the config is
     * unavailable instead of implying `braid.ups` is absent or disabled.
     * Scenario: hand-edited config JSON sets `ups.enable = true` but leaves
     * `mount_point` empty, so JSON parsing succeeds and schema validation fails.
     */
    #[test]
    fn valid_json_bad_schema_skips_ups_as_config_unavailable() {
        let f = write_temp(
            r#"{
                "mount_point": "",
                "ups": { "enable": true, "name": "ups" }
            }"#,
        );
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());

        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);

        let schema = find_check(&report, "config_schema");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(
            schema.message.contains("mount_point must not be empty"),
            "unexpected message: {}",
            schema.message
        );

        let ups_daemon = find_check(&report, "ups_daemon");
        assert_eq!(ups_daemon.status, CheckStatus::Skip);
        assert!(
            ups_daemon.message.contains("config not available"),
            "unexpected message: {}",
            ups_daemon.message
        );

        let braid_online = find_check(&report, "braid_online_active");
        assert_eq!(braid_online.status, CheckStatus::Skip);
        assert!(
            braid_online.message.contains("config not available"),
            "unexpected message: {}",
            braid_online.message
        );
    }

    #[test]
    fn overall_status_worst_wins() {
        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Ok,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Warn,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Warn);

        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Warn,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Fail);
    }

    #[test]
    fn skip_does_not_affect_overall() {
        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Ok,
                message: "".into(),
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Skip,
                message: "".into(),
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Ok);
    }

    #[test]
    fn json_serialization_lowercase() {
        let report = DoctorReport {
            status: CheckStatus::Ok,
            checks: vec![CheckResult {
                name: "test".into(),
                status: CheckStatus::Fail,
                message: "msg".into(),
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""status":"ok""#), "overall: {json}");
        assert!(json.contains(r#""status":"fail""#), "check: {json}");
        assert!(!json.contains("Ok"));
        assert!(!json.contains("Fail"));
    }

    #[test]
    fn human_format_contains_tags() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        let human = format_doctor_human(&report);
        assert!(human.contains("[ok]"), "expected [ok] tag:\n{human}");
        assert!(
            human.contains("config file"),
            "expected 'config file':\n{human}"
        );
        assert!(
            human.contains("config schema"),
            "expected 'config schema':\n{human}"
        );
    }

    #[test]
    fn human_format_fail_tag() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            human_options(),
        );
        let human = format_doctor_human(&report);
        assert!(human.contains("[fail]"), "expected [fail] tag:\n{human}");
        assert!(human.contains("[skip]"), "expected [skip] tag:\n{human}");
    }

    /* Intent: color-aware doctor output wraps each status tag and
     * leaves the label/message columns untouched.
     * Why it exists: `braid doctor` human output is a direct stdout
     * renderer, separate from the shared Preview path.
     * Scenario: one check at each severity is rendered with color
     * enabled and compared byte-for-byte.
     */
    #[test]
    fn human_format_with_colors_wraps_only_tags() {
        let report = DoctorReport {
            status: CheckStatus::Fail,
            checks: vec![
                CheckResult {
                    name: "config_file".into(),
                    status: CheckStatus::Ok,
                    message: "present".into(),
                },
                CheckResult {
                    name: "config_permissions".into(),
                    status: CheckStatus::Warn,
                    message: "world-writable".into(),
                },
                CheckResult {
                    name: "declared_disks".into(),
                    status: CheckStatus::Fail,
                    message: "missing disk1".into(),
                },
                CheckResult {
                    name: "pool_missing_devices".into(),
                    status: CheckStatus::Skip,
                    message: "pool offline".into(),
                },
            ],
        };
        let human = format_doctor_human_with(&report, true);
        let expected = "\
\x1b[32m[ok]\x1b[0m   config file     present
\x1b[33m[warn]\x1b[0m config perms    world-writable
\x1b[31m[fail]\x1b[0m declared disks  missing disk1
\x1b[90m[skip]\x1b[0m missing devs    pool offline
";
        assert_eq!(human, expected);
    }

    #[test]
    fn permissions_world_writable_warns() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Warn);
        assert!(perm.message.contains("world-writable"), "{}", perm.message);
        assert!(perm.message.contains("group-writable"), "{}", perm.message);
    }

    #[test]
    fn permissions_restrictive_ok() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        let perm = find_check(&report, "config_permissions");
        // May still warn about uid (tests don't run as root), but should not
        // warn about world/group bits.
        assert!(
            !perm.message.contains("world-"),
            "unexpected world- warning: {}",
            perm.message
        );
        assert!(
            !perm.message.contains("group-writable"),
            "unexpected group-writable: {}",
            perm.message
        );
    }

    #[test]
    fn permissions_skip_when_no_config() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            human_options(),
        );
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Skip);
    }

    #[test]
    fn human_format_contains_perms_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("config perms"),
            "expected 'config perms':\n{human}"
        );
    }

    #[test]
    fn declared_disks_skips_when_no_membership() {
        let f = write_temp(r#"{"mount_point":"/mnt/storage"}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths, human_options());
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_no_config() {
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &paths,
            human_options(),
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_bad_schema() {
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths, human_options());
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // --- summarize_declared_disks: pure rendering tests ---
    //
    // These tests target the pure summarizer directly, building DiskState
    // classifications by hand. They never touch the filesystem, the runner,
    // or StatePaths — by design, since the impure classifier is exercised by
    // the VM test in tests/cli/braid-doctor.py.

    fn cls(name: &str, by_id: &str, state: DiskState) -> (String, String, DiskState) {
        (name.to_owned(), by_id.to_owned(), state)
    }

    #[test]
    fn summarize_ok_when_all_headers_intact() {
        /*
         * Intent: when every declared disk passes both LUKS probes, the check
         *   returns Ok with the existing "all N declared disk(s) present"
         *   message.
         * Why it exists: protects the happy path against regressions introduced
         *   by extracting DiskState classification from the original check.
         * Scenario: a healthy multi-disk NAS with no header damage on any drive.
         */
        let inputs = [
            cls("disk1", "/dev/disk/by-id/wwn-0x1", DiskState::LuksHeaderOk),
            cls("disk2", "/dev/disk/by-id/wwn-0x2", DiskState::LuksHeaderOk),
        ];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Ok);
        assert_eq!(result.message, "all 2 declared disk(s) present");
    }

    #[test]
    fn summarize_warn_luks_header_unreadable() {
        /*
         * Intent: when a disk's LUKS header cannot even be recognized as LUKS
         *   (isLuks fails), the check warns and points the user at an
         *   off-system header backup with luksHeaderRestore — never at a
         *   local /var/lib/braid/luks-headers/ file.
         * Why it exists: this is the worst recoverable state — the on-disk
         *   header is gone or zeroed. Without specific guidance, users see a
         *   generic exit code from later cryptsetup operations and have no
         *   actionable next step. The negative assertions also pin the
         *   cross-command product invariant: braid status and the TUI already
         *   warn about persistent local .luksheader files, and doctor must be
         *   consistent with that posture rather than directing users at them.
         * Scenario: an HDD whose first sectors got clobbered by a misdirected
         *   dd or a controller bug — the dm-crypt mapping in kernel may still
         *   be active, but cryptsetup probes against the raw device fail.
         */
        let inputs = [cls(
            "disk1",
            "/dev/disk/by-id/wwn-0xABCD",
            DiskState::LuksHeaderUnreadable,
        )];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(msg.contains("off-system"), "missing 'off-system': {msg}");
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        // Cross-command consistency invariant.
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "doctor must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "doctor must not reference local .luksheader files: {msg}"
        );
    }

    #[test]
    fn summarize_warn_luks_header_damaged() {
        /*
         * Intent: when a disk has LUKS magic intact but luksDump fails, the
         *   check warns and suggests `cryptsetup repair --type luks2` with an
         *   explicit "make a safe backup first" warning.
         * Why it exists: this is the less-severe LUKS-corruption case — one
         *   header copy or some metadata field is bad but the magic is still
         *   there. The right tool is `cryptsetup repair`, but it mutates the
         *   header, so users must back up first. Negative assertions also
         *   pin the no-local-backup-references invariant.
         * Scenario: a disk with one corrupted LUKS2 header copy (the on-disk
         *   format keeps two copies for redundancy), or damaged keyslot
         *   metadata.
         */
        let inputs = [cls(
            "disk1",
            "/dev/disk/by-id/wwn-0xCAFE",
            DiskState::LuksHeaderDamaged,
        )];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("metadata damaged"),
            "missing 'metadata damaged': {msg}"
        );
        assert!(
            msg.contains("cryptsetup repair --type luks2"),
            "missing repair command: {msg}"
        );
        assert!(
            msg.contains("safe backup"),
            "missing safe-backup warning: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "doctor must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "doctor must not reference local .luksheader files: {msg}"
        );
    }

    #[test]
    fn summarize_warn_probe_failed_does_not_suggest_repair() {
        /*
         * Intent: when the cryptsetup probe itself fails to execute (Err from
         *   the runner — e.g. binary missing, IPC failure), the check reports
         *   the tooling problem but must NOT suggest `cryptsetup repair` or
         *   `luksHeaderRestore`.
         * Why it exists: conflating execution failure with header corruption
         *   could tell users to repair or restore a healthy disk. This test is
         *   the executable form of that invariant.
         * Scenario: cryptsetup binary missing from PATH on a misconfigured
         *   machine, or any other runner-level execution error.
         */
        let inputs = [cls(
            "disk1",
            "/dev/disk/by-id/wwn-0x1",
            DiskState::ProbeFailed("simulated runner error".into()),
        )];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("simulated runner error"),
            "missing error string: {msg}"
        );
        assert!(
            !msg.contains("cryptsetup repair"),
            "execution failure must not suggest repair: {msg}"
        );
        assert!(
            !msg.contains("luksHeaderRestore"),
            "execution failure must not suggest restore: {msg}"
        );
    }

    #[test]
    fn summarize_preserves_missing_and_not_block_messages() {
        /*
         * Intent: the existing "not found" and "not a block device"
         *   classifications continue to render with the same phrasing they had
         *   before the refactor.
         * Why it exists: protects against accidental wording regressions in
         *   the categories that were already covered by the original check.
         * Scenario: a NAS where one declared disk's /dev/disk/by-id/ symlink
         *   is gone (cabling issue) and another points at a regular file
         *   (config bug).
         */
        let inputs = [
            cls("disk1", "/dev/disk/by-id/wwn-0x1", DiskState::Missing),
            cls("disk2", "/dev/disk/by-id/wwn-0x2", DiskState::NotBlock),
            cls("disk3", "/dev/disk/by-id/wwn-0x3", DiskState::LuksHeaderOk),
        ];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("not found"), "missing 'not found': {msg}");
        assert!(
            msg.contains("not a block device"),
            "missing 'not a block device': {msg}"
        );
        assert!(msg.contains("disk1"), "missing disk1: {msg}");
        assert!(msg.contains("disk2"), "missing disk2: {msg}");
    }

    #[test]
    fn summarize_mixed_states_reports_all() {
        /*
         * Intent: when multiple disks fail in different ways, every failing
         *   disk's name appears in the message and the count is correct.
         * Why it exists: a real failure scenario rarely involves a single
         *   category; the check must aggregate findings instead of reporting
         *   only the first.
         * Scenario: a degraded NAS with one missing disk, one with an
         *   unreadable LUKS header, and one with damaged LUKS metadata
         *   simultaneously.
         */
        let inputs = [
            cls("disk1", "/dev/disk/by-id/wwn-0x1", DiskState::Missing),
            cls(
                "disk2",
                "/dev/disk/by-id/wwn-0x2",
                DiskState::LuksHeaderUnreadable,
            ),
            cls(
                "disk3",
                "/dev/disk/by-id/wwn-0x3",
                DiskState::LuksHeaderDamaged,
            ),
            cls("disk4", "/dev/disk/by-id/wwn-0x4", DiskState::LuksHeaderOk),
        ];
        let result = summarize_declared_disks(&inputs);
        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("3/4"), "expected '3/4' problem count: {msg}");
        assert!(msg.contains("disk1"), "missing disk1: {msg}");
        assert!(msg.contains("disk2"), "missing disk2: {msg}");
        assert!(msg.contains("disk3"), "missing disk3: {msg}");
    }

    #[test]
    fn human_format_contains_declared_disks_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, human_options());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("declared disks"),
            "expected 'declared disks':\n{human}"
        );
    }

    // --- data_profile_mismatch tests ---

    fn mountpoint_ok() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "mountpoint -q /mnt/storage".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "mountpoint -q /mnt/storage".into(),
                stdout: String::new(),
                stderr: "/mnt/storage is not a mountpoint\n".into(),
                exit_status: 1,
            },
        )
    }

    fn df_json(json: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs --format json filesystem df /mnt/storage".into(),
                stdout: json.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn df_json_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs --format json filesystem df /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem".into(),
                exit_status: 1,
            },
        )
    }

    const DF_RAID1_CLEAN: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    const DF_MIXED: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "Data", "bg-profile": "single", "total": 8388608, "used": 4194304 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    #[test]
    fn data_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("RAID1"),
            "expected RAID1 in message: {}",
            check.message
        );
    }

    #[test]
    fn data_profile_mixed_warns() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("mixed"),
            "expected 'mixed' in message: {}",
            check.message
        );
        assert!(
            check.message.contains("-dconvert=raid1,soft"),
            "expected soft flag in suggestion: {}",
            check.message
        );
    }

    #[test]
    fn data_profile_global_reserve_single_not_warned() {
        // GlobalReserve is always "single" — must not trigger mismatch
        let json = r#"{
            "filesystem-df": [
                { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
                { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
            ]
        }"#;
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(json);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn data_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(
            check.message.contains("config not available"),
            "{}",
            check.message
        );
    }

    #[test]
    fn data_profile_skip_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    #[test]
    fn data_profile_warn_when_df_fails() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json_fail();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not"),
            "expected error message: {}",
            check.message
        );
    }

    /* Intent: data_profile_mismatch reports malformed df JSON as a parse warning.
     * Why it exists: the shared df snapshot must preserve parser errors distinctly
     * from an unmounted pool or unavailable config.
     * Scenario: btrfs exits successfully but emits output that no longer matches
     * braid's expected `filesystem df --format json` schema.
     */
    #[test]
    fn data_profile_warn_when_df_json_malformed() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json("{not json");
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not parse data profiles"),
            "expected parse warning: {}",
            check.message
        );
    }

    // --- metadata_profile_mismatch tests ---

    const DF_MIXED_METADATA: &str = r#"{
        "filesystem-df": [
            { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
            { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
            { "bg-type": "Metadata", "bg-profile": "single", "total": 8388608, "used": 65536 },
            { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 },
            { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3407872, "used": 0 }
        ]
    }"#;

    // Intent: Verify metadata_profile_mismatch reports Ok for uniform RAID1 metadata.
    // Why: Ensures the check doesn't false-positive on a healthy pool.
    // Scenario: A clean 2-disk RAID1 pool has all metadata block groups as RAID1.
    #[test]
    fn metadata_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("RAID1"),
            "expected RAID1 in message: {}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch detects mixed metadata profiles.
    // Why: Mixed metadata is more dangerous than mixed data — metadata loss
    //   can make the entire filesystem unrecoverable.
    // Scenario: An interrupted `btrfs balance` leaves some metadata block groups
    //   as single while others remain RAID1. braid doctor should warn.
    #[test]
    fn metadata_profile_mixed_warns() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED_METADATA);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("mixed"),
            "expected 'mixed' in message: {}",
            check.message
        );
        assert!(
            check.message.contains("-dconvert=raid1,soft"),
            "expected soft flag in suggestion: {}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch skips when config is unavailable.
    // Why: Without config, we don't know the mount point to query.
    // Scenario: User runs `braid doctor --config /nonexistent` — profile checks
    //   should skip gracefully.
    #[test]
    fn metadata_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(
            check.message.contains("config not available"),
            "{}",
            check.message
        );
    }

    // Intent: Verify metadata_profile_mismatch skips when pool is not mounted.
    // Why: Can't query btrfs filesystem df on an unmounted pool.
    // Scenario: User runs `braid doctor` before unlocking the pool.
    #[test]
    fn metadata_profile_skip_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    // Intent: Verify human format includes the "meta profiles" label.
    // Why: Ensures the new check has a human-readable label in format_doctor_human.
    // Scenario: Operator reads `braid doctor` output and sees metadata profile status.
    #[test]
    fn human_format_contains_meta_profiles_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("meta profiles"),
            "expected 'meta profiles':\n{human}"
        );
    }

    // --- pool_missing_devices tests ---

    fn device_usage_healthy() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn device_usage_with_missing() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-toshiba, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:                  0

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    #[derive(Default)]
    struct PoolMissingDevicesRunner {
        calls: Mutex<Vec<CmdRequest>>,
    }

    impl CommandRunner for PoolMissingDevicesRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.calls.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::MountpointCheck { path } if path.0 == "/mnt/storage" => {
                    Ok(mountpoint_ok().1)
                }
                CmdRequest::BtrfsDeviceUsageRaw { mount_point }
                    if mount_point.0 == "/mnt/storage" =>
                {
                    Ok(device_usage_healthy().1)
                }
                CmdRequest::BtrfsFilesystemDfJson { .. } => {
                    panic!("pool_missing_devices must not query filesystem df")
                }
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            _request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            Err(CmdError::MissingMock)
        }
    }

    fn parsed_doctor_ctx<'a, R: CommandRunner>(
        runner: &'a R,
        paths: &'a StatePaths,
    ) -> DoctorContext<'a, R> {
        let value: serde_json::Value =
            serde_json::from_str(valid_config_json()).expect("test config JSON parses");
        let config: Config = serde_json::from_value(value.clone()).expect("test config parses");
        DoctorContext {
            config_path: PathBuf::new(),
            config_value: Some(value),
            config: Some(config),
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }

    // Intent: pool_missing_devices reports Ok when no devices are missing.
    // Why: ensures the check doesn't false-positive on a healthy pool.
    // Scenario: healthy 1-disk pool, all present.
    #[test]
    fn pool_missing_devices_ok_when_healthy() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.message.contains("no missing"), "{}", check.message);
    }

    /* Intent: pool_missing_devices can run without querying `btrfs filesystem df`.
     * Why it exists: missing-device detection only needs the mountpoint state and
     * `btrfs device usage`; tying it to df makes an unrelated parser or command
     * failure hide the more specific device probe.
     * Scenario: the pool is mounted and healthy, while the df command would fail
     * if this check accidentally requested it.
     */
    #[test]
    fn pool_missing_devices_does_not_require_filesystem_df() {
        let runner = PoolMissingDevicesRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);

        let check = check_pool_missing_devices(&mut ctx);

        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.message, "no missing devices");

        let calls = runner.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::MountpointCheck { .. })),
            "expected mountpoint probe, got: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
            "expected device usage probe, got: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsFilesystemDfJson { .. })),
            "missing-device check must not request df, got: {calls:?}"
        );
    }

    // Intent: pool_missing_devices warns when devices are missing and recommends replace.
    // Why: degraded pools need operator action; doctor should guide them to replace.
    // Scenario: one drive died in a 2-disk NAS.
    #[test]
    fn pool_missing_devices_warns_with_replace_recommendation() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_with_missing();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("missing device"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("braid replace"),
            "expected replace recommendation: {}",
            check.message
        );
        assert!(
            check.message.contains("--missing-id"),
            "expected --missing-id in recommendation: {}",
            check.message
        );
        assert!(
            check.message.contains("devid"),
            "expected devid in message: {}",
            check.message
        );
    }

    // Intent: pool_missing_devices skips when pool is not mounted.
    // Why: can't probe device usage on an unmounted pool.
    // Scenario: user runs braid doctor before unlocking.
    #[test]
    fn pool_missing_devices_skip_when_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = mock().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // Intent: human format includes the "missing devs" label.
    // Why: ensures the new check has a human-readable label.
    // Scenario: operator reads braid doctor output.
    #[test]
    fn human_format_contains_missing_devs_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = mock()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("missing devs"),
            "expected 'missing devs':\n{human}"
        );
    }

    // -----------------------------------------------------------------------
    // check_beep_path_inner — deterministic branch coverage
    //
    // All beep_path tests target the inner helper directly, passing both the
    // notifier-config path and the is_root flag explicitly. This isolates the
    // check from `geteuid()` so the same tests pass regardless of whether
    // `cargo test` is invoked as root or as an unprivileged user. The runner
    // is mocked via MockRunner::with_output for the success/failure branches.
    // -----------------------------------------------------------------------

    fn beep_ctx<'a, R: CommandRunner>(
        runner: &'a R,
        paths: &'a StatePaths,
    ) -> DoctorContext<'a, R> {
        DoctorContext {
            config_path: PathBuf::new(),
            config_value: None,
            config: None,
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }

    // Intent: when the notifier config file does not exist, the check skips
    //   with a clear message that points at the missing braid monitor.
    // Why: a bare braid install (no monitor module imported) must produce
    //   Skip, not Fail — Fail would generate noise on every doctor run on
    //   non-NAS hosts where braid happens to be installed for inspection.
    // Scenario: developer machine running `braid doctor` against a config
    //   without the NixOS monitor module enabled.
    #[test]
    fn beep_path_skips_when_notifier_config_absent() {
        let (_dir, paths) = isolated_paths();
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            Path::new("/tmp/nonexistent-braid-notifier-config-doctor-test.json"),
            beep_check_options(
                true,  // is_root: irrelevant since the file doesn't exist
                false, // json_output: irrelevant since the file doesn't exist
                false, // play_beep: irrelevant since the file doesn't exist
            ),
        );
        assert_eq!(result.name, "beep_path");
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.contains("braid monitor not configured"),
            "unexpected: {}",
            result.message
        );
    }

    // Intent: malformed notifier config produces Fail (not Skip), because a
    //   broken config file is a real defect: the NixOS module wrote junk.
    // Why: silently skipping on malformed JSON would mask a regression in the
    //   module's `builtins.toJSON` writer. Loud failure forces a fix.
    // Scenario: a future refactor of monitor.nix accidentally writes invalid
    //   JSON to /etc/braid/notifier-config.json.
    #[test]
    fn beep_path_fail_on_malformed_config() {
        let f = write_temp("not json {");
        let (_dir, paths) = isolated_paths();
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(true, false, false));
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.message.contains("malformed"),
            "unexpected: {}",
            result.message
        );
    }

    // Intent: when monitor.beep is disabled (`beep_probe_path: null`), the
    //   check skips with the "beep monitoring disabled" message.
    // Why: users who explicitly opt out of beep alerting must not see a
    //   misleading Fail or Warn — they have intentionally disabled the
    //   feature, so absence of a beep is correct behavior.
    // Scenario: NAS user who prefers email or webhook alerts and has set
    //   `braid.monitor.beep = false` in their NixOS configuration.
    #[test]
    fn beep_path_skips_when_beep_disabled() {
        let f = write_temp(r#"{"beep_probe_path": null}"#);
        let (_dir, paths) = isolated_paths();
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(true, false, false));
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.contains("beep monitoring disabled"),
            "unexpected: {}",
            result.message
        );
    }

    // Intent: when invoked without root, the check skips with a clear
    //   "requires root" message AND does not invoke the runner at all.
    // Why: lack of root is an INVOCATION CONTEXT issue, not a SPEAKER
    //   HEALTH issue. Reporting Fail here would conflate "you ran doctor
    //   without sudo" with "your speaker is broken" — making doctor
    //   untrustworthy and less scriptable. The runner-not-invoked
    //   assertion is enforced implicitly: MockRunner returns MissingMock
    //   for any unmatched call, which would surface as a Fail rather
    //   than a Skip. This test would catch any regression that probes
    //   the wrapper before checking root.
    // Scenario: unprivileged user runs `braid doctor` (without sudo) on
    //   a real NAS where beep is enabled.
    #[test]
    fn beep_path_skips_when_not_root() {
        let f = write_temp(r#"{"beep_probe_path": "/nix/store/fake/bin/braid-beep-probe"}"#);
        let (_dir, paths) = isolated_paths();
        // No BraidBeepProbe output configured: if the check tries to run
        // the wrapper, MockRunner returns MissingMock, which becomes a
        // Fail message — pinning the runner-not-invoked invariant.
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(false, false, true));
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.contains("requires root"),
            "unexpected: {}",
            result.message
        );
        assert!(
            result.message.contains("alert test beep"),
            "skip message must mention the alert test beep (product framing): {}",
            result.message
        );
    }

    // Intent: when invoked in --json mode, the check skips with a clear
    //   "json mode" message AND does not invoke the runner at all, even
    //   when is_root=true, play_beep=true, and a real-looking probe path is
    //   configured.
    // Why: `braid doctor --json` is for programmatic consumption — emitting
    //   an audible side effect from a data-output command would surprise
    //   any script piping doctor's JSON into a monitoring system. The
    //   runner-not-invoked invariant is enforced implicitly: MockRunner
    //   returns MissingMock for any unmatched call, so a regression that
    //   spawned the wrapper before checking the json gate would surface
    //   as a Fail rather than a Skip.
    // Scenario: an oncall engineer pipes `braid doctor --json | jq` from
    //   a remote shell to inspect health, expecting silence.
    #[test]
    fn beep_path_skips_in_json_mode() {
        let f = write_temp(r#"{"beep_probe_path": "/nix/store/fake/bin/braid-beep-probe"}"#);
        let (_dir, paths) = isolated_paths();
        // No BraidBeepProbe output configured: if the check tries to run
        // the wrapper, MockRunner returns MissingMock, which becomes a
        // Fail message — pinning the runner-not-invoked invariant.
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            beep_check_options(
                true, // is_root: yes
                true, // json_output: yes
                true, // play_beep: yes, but --json still suppresses it
            ),
        );
        assert_eq!(result.status, CheckStatus::Skip);
        assert_eq!(
            result.message,
            "skipped in --json mode -- rerun with --beep without --json to play the alert test beep"
        );
    }

    // Intent: plain doctor skips the audible beep check by default and does
    //   not invoke the runner.
    // Why: the alert sound must be opt-in so ordinary diagnostics stay quiet.
    //   MockRunner has no BraidBeepProbe output; if the wrapper were invoked,
    //   MissingMock would turn this into Fail instead of Skip.
    // Scenario: operator runs `sudo braid doctor` on a NAS where beep alerting
    //   is configured but does not want to play the alert sound.
    #[test]
    fn beep_path_skips_by_default_without_invoking_runner() {
        let f = write_temp(r#"{"beep_probe_path": "/nix/store/fake/bin/braid-beep-probe"}"#);
        let (_dir, paths) = isolated_paths();
        let runner = mock();
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(true, false, false));
        assert_eq!(result.status, CheckStatus::Skip);
        assert_eq!(
            result.message,
            "skipped (pass --beep to play the audible alert test beep)"
        );
    }

    // Intent: when --beep is explicit and the wrapper exits 0, the check
    //   returns Ok and the message tells the operator what they should have
    //   heard.
    // Why: pins the opt-in alert-preview framing in the user-facing copy.
    // Scenario: healthy NAS, root user, `braid doctor --beep` plays the beep
    //   end to end and reports success.
    #[test]
    fn beep_path_ok_on_zero_exit() {
        let probe_path = "/nix/store/fake/bin/braid-beep-probe";
        let f = write_temp(&format!(r#"{{"beep_probe_path": "{probe_path}"}}"#));
        let (_dir, paths) = isolated_paths();
        let runner = mock().with_output(
            CmdRequest::BraidBeepProbe {
                path: probe_path.into(),
            },
            RawCommandOutput {
                cmd: "braid-beep-probe".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(true, false, true));
        assert_eq!(result.status, CheckStatus::Ok);
        assert_eq!(
            result.message,
            "alert test beep command succeeded -- you should have heard a 1 kHz, 500 ms disk-alert beep"
        );
    }

    // Intent: when the wrapper exits non-zero, the check returns Fail and
    //   the message both names the user-visible problem (could not play the
    //   beep) AND retains diagnostic detail ("speaker likely broken" plus
    //   the wrapper's stderr).
    // Why: a broken PC speaker silently swallows alerts in production.
    // Doctor exists to surface that condition with enough context for the
    //   operator to act on it without having to dig into journalctl.
    // Scenario: NAS where pcspkr blacklist is still active or evdev udev
    //   rule is missing — the wrapper fails fast when invoked.
    #[test]
    fn beep_path_fail_on_nonzero_exit() {
        let probe_path = "/nix/store/fake/bin/braid-beep-probe";
        let f = write_temp(&format!(r#"{{"beep_probe_path": "{probe_path}"}}"#));
        let (_dir, paths) = isolated_paths();
        let runner = mock().with_output(
            CmdRequest::BraidBeepProbe {
                path: probe_path.into(),
            },
            RawCommandOutput {
                cmd: "braid-beep-probe".into(),
                stdout: String::new(),
                stderr: "mock failure".into(),
                exit_status: 1,
            },
        );
        let mut ctx = beep_ctx(&runner, &paths);
        let result =
            check_beep_path_inner(&mut ctx, f.path(), beep_check_options(true, false, true));
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result.message.contains("could not play alert test beep"),
            "Fail message must lead with the user-visible problem: {}",
            result.message
        );
        assert!(
            result.message.contains("speaker likely broken"),
            "Fail message must retain the diagnostic hint: {}",
            result.message
        );
        assert!(
            result.message.contains("mock failure"),
            "Fail message must include wrapper stderr: {}",
            result.message
        );
    }

    // ---------------------------------------------------------------------
    // UPS doctor checks
    // ---------------------------------------------------------------------

    fn ups_ctx<'a, R: CommandRunner>(
        runner: &'a R,
        paths: &'a StatePaths,
        config_json: &str,
    ) -> DoctorContext<'a, R> {
        let config: Option<Config> = serde_json::from_str(config_json).ok();
        DoctorContext {
            config_path: PathBuf::new(),
            config_value: Some(serde_json::from_str(config_json).expect("test config parses")),
            config,
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }

    fn config_with_ups_enabled() -> &'static str {
        r#"{"mount_point":"/mnt/storage","ups":{"enable":true,"name":"ups"}}"#
    }

    fn config_without_ups() -> &'static str {
        r#"{"mount_point":"/mnt/storage"}"#
    }

    fn config_with_ups_disabled() -> &'static str {
        r#"{"mount_point":"/mnt/storage","ups":{"enable":false,"name":"ups"}}"#
    }

    fn systemctl_is_active_output(state: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "systemctl is-active braid-online.service".into(),
            stdout: if state.is_empty() {
                String::new()
            } else {
                format!("{state}\n")
            },
            stderr: String::new(),
            exit_status: match state {
                "active" | "reloading" | "refreshing" => 0,
                _ => 3,
            },
        }
    }

    // Intent: check_ups_daemon_up reports Ok when upsc returns a healthy
    // OL status.
    // Why: baseline happy path; confirms a live upsd does not trigger a
    // spurious Warn.
    // Scenario: operator runs `braid doctor` with UPS enabled and
    // upsd.service healthy.
    #[test]
    fn ups_daemon_check_ok_when_upsc_returns_ol() {
        let runner = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: "ups.status: OL\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Ok, "got: {r:?}");
        assert!(r.message.contains("reachable"));
    }

    // Intent: check_ups_daemon_up warns when `upsc` exits non-zero.
    // Why: the daemon-down state deserves a visible but non-fatal nudge
    // -- the plan says this is a Warn (operator fixes it, braid does
    // not intervene). Regression here would turn every rebooting
    // upsd.service into a false Fail that masks real problems.
    // Scenario: `systemctl stop upsd.service` while doctor runs.
    #[test]
    fn ups_daemon_check_warns_when_daemon_down() {
        let runner = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: String::new(),
                stderr: "Error: Connection refused".into(),
                exit_status: 1,
            },
        );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Warn, "got: {r:?}");
        assert!(r.message.contains("unreachable"));
    }

    // Intent: check_ups_daemon_up skips when braid.ups block is absent.
    // Why: host without UPS support must not see UPS-colored warnings
    // (both noise and misleading). Skipping also keeps the doctor
    // count stable for pre-UPS deployments.
    // Scenario: non-UPS deployment runs `braid doctor`.
    #[test]
    fn ups_daemon_check_skips_when_config_absent() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_without_ups());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(
            r.message.contains("braid.ups not enabled"),
            "unexpected message: {}",
            r.message
        );
    }

    // Intent: check_ups_daemon_up skips when braid.ups.enable = false.
    // Why: the user explicitly opted out; doctor should respect that
    // without complaining about a daemon they intentionally disabled.
    // Scenario: operator temporarily disabled UPS for maintenance.
    #[test]
    fn ups_daemon_check_skips_when_enable_false() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_disabled());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(
            r.message.contains("braid.ups not enabled"),
            "unexpected message: {}",
            r.message
        );
    }

    // Intent: check_braid_online_active_when_mounted fails (high
    // severity) when the pool is mounted under UPS but
    // braid-online.service is not active.
    // Why: THIS IS THE CRITICAL FAULT. Without braid-online, the
    // SHUTDOWNCMD path does not unmount the pool on LB, and the Plan
    // 1 safety guarantee silently breaks. Fail (not Warn) so the
    // operator sees the escalation.
    // Scenario: operator disabled braid-online.service temporarily and
    // forgot to re-enable it before an outage.
    #[test]
    fn braid_online_check_fails_when_inactive_and_mounted() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".into()),
                },
                RawCommandOutput {
                    cmd: "mountpoint".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::SystemctlIsActive {
                    unit: "braid-online.service".into(),
                },
                systemctl_is_active_output("inactive"),
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("inactive"));
        assert!(r.message.contains("UPS shutdown"));
    }

    // Intent: check_braid_online_active_when_mounted returns Ok when
    // braid-online.service is active.
    // Why: the happy path must be silent; any noise here and operators
    // learn to ignore the check.
    // Scenario: normal UPS-enabled deployment running smoothly.
    #[test]
    fn braid_online_check_ok_when_active() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".into()),
                },
                RawCommandOutput {
                    cmd: "mountpoint".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::SystemctlIsActive {
                    unit: "braid-online.service".into(),
                },
                systemctl_is_active_output("active"),
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    /* Intent: check_braid_online_active_when_mounted treats systemd's
     * is-active success states as Ok while the pool is mounted.
     * Why it exists: these states indicate braid-online.service has reached
     * the state where systemd can run its stop hook.
     * Scenario: operator runs `braid doctor` while systemd reports an active,
     * reloading, or refreshing braid-online.service.
     */
    #[test]
    fn braid_online_check_ok_when_settled_success_state() {
        for status in ["active", "reloading", "refreshing"] {
            let runner = MockRunner::default()
                .with_output(
                    CmdRequest::MountpointCheck {
                        path: MountPoint("/mnt/storage".into()),
                    },
                    RawCommandOutput {
                        cmd: "mountpoint".into(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_status: 0,
                    },
                )
                .with_output(
                    CmdRequest::SystemctlIsActive {
                        unit: "braid-online.service".into(),
                    },
                    systemctl_is_active_output(status),
                );
            let (_dir, paths) = isolated_paths();
            let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
            let r = check_braid_online_active_when_mounted(&mut ctx);
            assert_eq!(r.status, CheckStatus::Ok, "status={status}");
            assert!(r.message.contains(status), "message={}", r.message);
        }
    }

    /* Intent: check_braid_online_active_when_mounted warns when
     * braid-online.service is still activating.
     * Why it exists: activating is not safe enough for Ok because systemd
     * has not yet guaranteed ExecStop, but it is plausibly transient and
     * should not be escalated to Fail.
     * Scenario: the wrapper has just started braid-online.service and
     * `braid doctor` runs before systemd has finished the transition.
     */
    #[test]
    fn braid_online_check_warns_when_activating() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".into()),
                },
                RawCommandOutput {
                    cmd: "mountpoint".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::SystemctlIsActive {
                    unit: "braid-online.service".into(),
                },
                systemctl_is_active_output("activating"),
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());

        let r = check_braid_online_active_when_mounted(&mut ctx);

        assert_eq!(r.status, CheckStatus::Warn, "got: {r:?}");
        assert!(r.message.contains("activating"), "{}", r.message);
        assert_eq!(
            r.message,
            "braid-online.service is activating -- UPS shutdown hook is not confirmed yet; re-run braid doctor shortly"
        );
    }

    /* Intent: check_braid_online_active_when_mounted rejects every unsafe
     * systemctl boundary state while the pool is mounted under UPS.
     * Why it exists: only settled success states can prove the
     * braid-online.service shutdown hook is available.
     * Scenario: systemd reports a failed, stopping, unknown, empty, or
     * otherwise unrecognized unit state.
     */
    #[test]
    fn braid_online_check_fails_for_unsafe_systemctl_states() {
        for status in ["deactivating", "failed", "unknown", "", "bogus"] {
            let runner = MockRunner::default()
                .with_output(
                    CmdRequest::MountpointCheck {
                        path: MountPoint("/mnt/storage".into()),
                    },
                    RawCommandOutput {
                        cmd: "mountpoint".into(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_status: 0,
                    },
                )
                .with_output(
                    CmdRequest::SystemctlIsActive {
                        unit: "braid-online.service".into(),
                    },
                    systemctl_is_active_output(status),
                );
            let (_dir, paths) = isolated_paths();
            let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());

            let r = check_braid_online_active_when_mounted(&mut ctx);

            assert_eq!(r.status, CheckStatus::Fail, "status={status}, got: {r:?}");
            if !status.is_empty() {
                assert!(r.message.contains(status), "{}", r.message);
            }
            assert!(r.message.contains("UPS shutdown"), "{}", r.message);
            assert!(
                r.message.contains("systemctl start braid-online.service"),
                "{}",
                r.message
            );
            assert!(r.message.contains("braid unlock"), "{}", r.message);
        }
    }

    // Intent: check_braid_online_active_when_mounted skips when pool
    // is not mounted.
    // Why: braid-online only matters while the pool is online; a
    // locked pool has nothing to unmount. A Fail here would fire at
    // every boot while the user is still typing their passphrase.
    // Scenario: pre-unlock `braid doctor`.
    #[test]
    fn braid_online_check_skips_when_not_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".into()),
            },
            RawCommandOutput {
                cmd: "mountpoint".into(),
                stdout: String::new(),
                stderr: "not a mountpoint".into(),
                exit_status: 1,
            },
        );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Skip);
    }

    // Intent: check_braid_online_active_when_mounted skips when the
    // braid.ups block is absent.
    // Why: a deployment with no UPS configured must not see
    // braid-online/UPS safety warnings.
    // Scenario: non-UPS host runs `braid doctor` while the pool state is
    // irrelevant to UPS shutdown safety.
    #[test]
    fn braid_online_check_skips_when_ups_absent() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_without_ups());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(
            r.message.contains("braid.ups not enabled"),
            "unexpected message: {}",
            r.message
        );
    }

    // Intent: check_braid_online_active_when_mounted skips when UPS
    // is disabled.
    // Why: without UPS, braid-online is not the same safety
    // bottleneck; a Fail on a non-UPS host is just noise.
    // Scenario: host without UPS runs doctor.
    #[test]
    fn braid_online_check_skips_when_ups_disabled() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_disabled());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(
            r.message.contains("braid.ups not enabled"),
            "unexpected message: {}",
            r.message
        );
    }
}
