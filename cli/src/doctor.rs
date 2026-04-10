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
    df_snapshot: Option<DfSnapshot>,
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

fn ensure_df_snapshot<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) {
    if ctx.df_snapshot.is_some() {
        return;
    }

    let config = match &ctx.config {
        Some(c) => c,
        None => return,
    };

    let mount_point = config.mount_point().to_owned();

    // Check if pool is mounted
    match ctx.runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    }) {
        Ok(out) if out.exit_status == 0 => {}
        _ => {
            ctx.df_snapshot = Some(DfSnapshot::NotMounted);
            return;
        }
    }

    // Query btrfs filesystem df
    let raw = match ctx.runner.run(&CmdRequest::BtrfsFilesystemDfJson {
        mount_point: mount_point.clone(),
    }) {
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

    ensure_df_snapshot(ctx);

    match &ctx.df_snapshot {
        None | Some(DfSnapshot::NotMounted) => {
            return CheckResult {
                name: "pool_missing_devices".into(),
                status: CheckStatus::Skip,
                message: "skipped (pool not mounted)".into(),
            };
        }
        _ => {}
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
/// Plays a short alert test beep (1 kHz, 500 ms) via the canonical
/// `braid-beep-probe` wrapper — the same code path the alert service uses.
/// A successful run is *both* a notifier-health check and a positive
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
fn check_beep_path<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>, json: bool) -> CheckResult {
    let is_root = unsafe { libc::geteuid() } == 0;
    check_beep_path_inner(ctx, Path::new(NOTIFIER_CONFIG_PATH), is_root, json)
}

fn check_beep_path_inner<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    notifier_path: &Path,
    is_root: bool,
    json_output: bool,
) -> CheckResult {
    let name = "beep_path".to_string();

    // 1. Read the notifier config the NixOS module wrote. Absent → Skip.
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
    if !is_root {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (requires root to play the alert test beep)".into(),
        };
    }

    // 5. JSON mode is for programmatic consumption — emitting an audible
    //    side effect from a data-output command is wrong. The check still
    //    appears in the report (as Skip) so scripts auditing doctor output
    //    can see it; the wrapper is simply never invoked.
    if json_output {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped in --json mode -- run without --json to play \
                      the alert test beep"
                .into(),
        };
    }

    // 6. Run the canonical wrapper. This PLAYS the real short alert beep
    //    (1 kHz, 500 ms) — same code path the alert service uses. Hearing
    //    the beep is both the success signal AND a preview of what real
    //    disk alerts will sound like.
    match ctx
        .runner
        .run(&CmdRequest::BraidBeepProbe { path: probe_path })
    {
        Ok(out) if out.exit_status == 0 => CheckResult {
            name,
            status: CheckStatus::Ok,
            message: "alert test beep played (1 kHz, 500 ms) -- \
                      same beep braid will use for real disk alerts"
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
    json: bool,
) -> DoctorReport {
    let mut ctx = DoctorContext {
        config_path: config_path.to_owned(),
        config_value: None,
        config: None,
        runner,
        paths,
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
        check_beep_path(&mut ctx, json),
    ];

    let status = overall_status(&checks);

    DoctorReport { status, checks }
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

pub fn format_doctor_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    for c in &report.checks {
        let tag = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skip => "skip",
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
            other => other,
        };
        out.push_str(&format!("[{tag:<4}]  {label:<14}  {}\n", c.message));
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

pub fn cmd_doctor(config_path: &Path, paths: &StatePaths, json: bool) -> Result<(), DoctorError> {
    let runner = RealRunner;
    let report = run_doctor(config_path, &runner, paths, json);

    if json {
        // serde_json::to_string_pretty won't fail on our types
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(DoctorError::Serialize)?
        );
    } else {
        print!("{}", format_doctor_human(&report));
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
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;
    use crate::types::MountPoint;
    use std::io::Write;
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
        let report = run_doctor(f.path(), &mock(), &paths, false);
        assert_eq!(report.checks.len(), 8);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        // declared_disks skips since no pool membership file exists in test env
        assert_eq!(
            find_check(&report, "declared_disks").status,
            CheckStatus::Skip
        );
        // beep_path skips because /etc/braid/notifier-config.json does not
        // exist in the cargo-test environment.
        let beep = find_check(&report, "beep_path");
        assert_eq!(beep.status, CheckStatus::Skip);
        assert!(
            beep.message.contains("braid monitor not configured"),
            "expected 'braid monitor not configured' in: {}",
            beep.message
        );
    }

    #[test]
    fn missing_file_fail_skip() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            false,
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
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
    }

    #[test]
    fn valid_json_bad_schema_empty_mount() {
        let f = write_temp(r#"{"disks":{"a":{"by_id":"/dev/disk/by-id/a"}},"mount_point":""}"#);
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        let schema = find_check(&report, "config_schema");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(
            schema.message.contains("mount_point must not be empty"),
            "unexpected message: {}",
            schema.message
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
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
        let human = format_doctor_human(&report);
        assert!(human.contains("[ok  ]"), "expected [ok  ] tag:\n{human}");
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
            false,
        );
        let human = format_doctor_human(&report);
        assert!(human.contains("[FAIL]"), "expected [FAIL] tag:\n{human}");
        assert!(human.contains("[skip]"), "expected [skip] tag:\n{human}");
    }

    #[test]
    fn permissions_world_writable_warns() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
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
            false,
        );
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Skip);
    }

    #[test]
    fn human_format_contains_perms_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &mock(), &paths, false);
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
            false,
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_bad_schema() {
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(f.path(), &mock(), &paths, false);
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
        let report = run_doctor(f.path(), &mock(), &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn data_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &mock(),
            &isolated_paths().1,
            false,
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not"),
            "expected error message: {}",
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
            false,
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.message.contains("no missing"), "{}", check.message);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
        let report = run_doctor(f.path(), &runner, &isolated_paths().1, false);
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
            true,  // is_root: irrelevant since the file doesn't exist
            false, // json_output: irrelevant since the file doesn't exist
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
        let result = check_beep_path_inner(&mut ctx, f.path(), true, false);
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
        let result = check_beep_path_inner(&mut ctx, f.path(), true, false);
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
        let result = check_beep_path_inner(&mut ctx, f.path(), false, false);
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
    //   when is_root=true and a real-looking probe path is configured.
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
            true, // is_root: yes
            true, // json_output: yes
        );
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.to_lowercase().contains("json"),
            "skip message must mention --json mode: {}",
            result.message
        );
        assert!(
            result.message.contains("alert test beep"),
            "skip message must mention the alert test beep (product framing): {}",
            result.message
        );
    }

    // Intent: when the wrapper exits 0, the check returns Ok and the
    //   message explicitly mentions the alert test beep (product framing).
    // Why: pins the dual-purpose "health check + alert preview" framing in
    //   the user-facing copy. A regression that says "path invokable" or
    //   similar implementation language would silently degrade the framing
    //   even though the status is still Ok.
    // Scenario: healthy NAS, root user, `braid doctor` plays the beep end
    //   to end and reports success.
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
        let result = check_beep_path_inner(&mut ctx, f.path(), true, false);
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(
            result.message.contains("alert test beep"),
            "Ok message must mention the alert test beep: {}",
            result.message
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
        let result = check_beep_path_inner(&mut ctx, f.path(), true, false);
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
}
