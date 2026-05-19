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
use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::luks;
use crate::membership;
use crate::parse::types::{BtrfsBgType, BtrfsDfOutput, BtrfsProfile};
use crate::parse::{parse_btrfs_df_json, parse_cryptsetup_luks_uuid};
use crate::preflight;
use crate::probe::{self, Filesystem, ProbeError, RealFilesystem};
use crate::state_paths::StatePaths;
use crate::status::format_bytes;
use crate::status_tag::{StatusTag, color_enabled_for_stdout, status_line};
use crate::types::{LuksUuid, MountPoint, PoolState};

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

impl CheckResult {
    fn ok(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Ok,
            message: message.into(),
        }
    }

    fn warn(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
        }
    }

    fn fail(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
        }
    }

    fn skip(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skip,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub status: CheckStatus,
    pub checks: Vec<CheckResult>,
}

enum DfSnapshot {
    NotMounted,
    Error(String),
    Ok(BtrfsDfOutput),
}

/// Per-run state for `braid doctor`: caches mountpoint, df, and live-pool
/// probes across checks so the orchestrator avoids re-querying btrfs, and
/// threads the parsed config plus runner/filesystem borrows each check needs.
pub(crate) struct DoctorContext<'a, R: CommandRunner> {
    config_path: PathBuf,
    config_value: Option<serde_json::Value>,
    config: Option<Config>,
    runner: &'a R,
    fs: &'a dyn Filesystem,
    paths: &'a StatePaths,
    mountpoint_is_mounted: Option<bool>,
    df_snapshot: Option<DfSnapshot>,
    pool_state: Option<Result<PoolState, ProbeError>>,
}

#[derive(Debug, Clone, Copy)]
pub struct DoctorOptions {
    pub json: bool,
    pub beep: bool,
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
            return CheckResult::fail("config_file", format!("{}: {e}", path.display()));
        }
    };

    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => {
            ctx.config_value = Some(v);
            CheckResult::ok(
                "config_file",
                format!("{} exists and is valid JSON", path.display()),
            )
        }
        Err(e) => CheckResult::fail(
            "config_file",
            format!("{}: invalid JSON: {e}", path.display()),
        ),
    }
}

fn check_config_schema<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let value = match &ctx.config_value {
        Some(v) => v.clone(),
        None => {
            return CheckResult::skip("config_schema", "skipped (config file not available)");
        }
    };

    let cfg: Config = match serde_json::from_value(value) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::fail(
                "config_schema",
                format!("failed to deserialize config: {e}"),
            );
        }
    };

    ctx.config = Some(cfg);
    CheckResult::ok("config_schema", "required fields present and valid")
}

fn check_config_permissions<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config_value.is_none() {
        return CheckResult::skip("config_permissions", "skipped (config file not available)");
    }

    if ctx.config_path.as_os_str() != std::ffi::OsStr::new(DEFAULT_CONFIG_PATH) {
        return CheckResult::skip("config_permissions", "skipped (custom config path)");
    }

    check_config_permissions_for_path(&ctx.config_path)
}

fn check_config_permissions_for_path(path: &Path) -> CheckResult {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return CheckResult::warn(
                "config_permissions",
                format!("could not stat {}: {e}", path.display()),
            );
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
        CheckResult::ok(
            "config_permissions",
            format!("{} permissions ok", path.display()),
        )
    } else {
        CheckResult::warn(
            "config_permissions",
            format!("{}: {}", path.display(), warnings.join(", ")),
        )
    }
}

/// Classification of a single declared disk after the doctor's LUKS probe.
/// `summarize_declared_disks` translates a slice of these into a `CheckResult`;
/// the variants pin the six reachable outcomes (header Ok, UUID mismatch,
/// header unreadable, header damaged, missing/non-block/probe-failed).
#[derive(Debug, Clone)]
pub(crate) enum DiskState {
    /// Header probes succeeded and the live LUKS UUID matched the pool.json key.
    LuksHeaderOk,
    /// `cryptsetup isLuks`, `cryptsetup luksDump`, and `cryptsetup luksUUID`
    /// succeeded, but the live LUKS UUID does not match the pool.json key.
    LuksUuidMismatch {
        expected: LuksUuid,
        observed: LuksUuid,
    },
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
fn classify_disk_state<R: CommandRunner>(
    runner: &R,
    path: &Path,
    expected_uuid: &LuksUuid,
) -> DiskState {
    match std::fs::metadata(path) {
        Err(_) => return DiskState::Missing,
        Ok(meta) if !meta.file_type().is_block_device() => return DiskState::NotBlock,
        Ok(_) => {}
    }

    let device = path.to_string_lossy().into_owned();
    classify_luks_identity(runner, &device, expected_uuid)
}

/// Runner-only LUKS identity classifier so unit tests can cover the UUID
/// comparison without depending on host block-device state.
fn classify_luks_identity<R: CommandRunner>(
    runner: &R,
    device: &str,
    expected_uuid: &LuksUuid,
) -> DiskState {
    match luks::probe_luks_header(runner, device) {
        luks::LuksHeaderState::Unreadable => return DiskState::LuksHeaderUnreadable,
        luks::LuksHeaderState::Damaged => return DiskState::LuksHeaderDamaged,
        luks::LuksHeaderState::ProbeFailed(err) => return DiskState::ProbeFailed(err),
        luks::LuksHeaderState::Ok => {}
    }

    let raw = match runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: device.to_owned(),
    }) {
        Ok(raw) => raw,
        Err(e) => return DiskState::ProbeFailed(e.to_string()),
    };
    let observed = match parse_cryptsetup_luks_uuid(&raw) {
        Ok(out) => out.uuid,
        Err(e) => return DiskState::ProbeFailed(e.to_string()),
    };

    if observed == *expected_uuid {
        DiskState::LuksHeaderOk
    } else {
        DiskState::LuksUuidMismatch {
            expected: expected_uuid.clone(),
            observed,
        }
    }
}

/// Pure rendering function: takes pre-classified per-disk states and returns
/// the `CheckResult` for `declared_disks`.
///
/// Remediation messages delegate to `luks::*_guidance` helpers shared with
/// the unlock and enroll error-enrichment paths. Those helpers enforce the
/// cross-command invariant that no user-facing header recovery message
/// references local
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
    let mut uuid_mismatch: Vec<String> = Vec::new();

    for (name, by_id, state) in classifications {
        match state {
            DiskState::LuksHeaderOk => {}
            DiskState::LuksUuidMismatch { expected, observed } => {
                uuid_mismatch.push(format!(
                    "{name} ({by_id}): expected {expected}, observed {observed} -- {}",
                    luks::luks_uuid_mismatch_guidance()
                ));
            }
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
        + header_damaged.len()
        + uuid_mismatch.len();

    if problem_count == 0 {
        return CheckResult::ok(
            "declared_disks",
            format!(
                "all {total} declared {} present",
                if total == 1 { "disk" } else { "disks" }
            ),
        );
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
    if !uuid_mismatch.is_empty() {
        parts.push(format!(
            "{} with LUKS UUID mismatch: {}",
            uuid_mismatch.len(),
            uuid_mismatch.join("; ")
        ));
    }
    if !probe_failed.is_empty() {
        parts.push(format!(
            "{} with LUKS header probe failures: {}",
            probe_failed.len(),
            probe_failed.join("; ")
        ));
    }

    let message = format!(
        "{}/{} {} problems: {}",
        problem_count,
        total,
        if total == 1 { "disk has" } else { "disks have" },
        parts.join("; ")
    );
    if uuid_mismatch.is_empty() {
        CheckResult::warn("declared_disks", message)
    } else {
        CheckResult::fail("declared_disks", message)
    }
}

fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let pool_membership = match membership::load_membership(ctx.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return CheckResult::skip("declared_disks", "skipped (no pool membership file)");
        }
        Err(e) => {
            return CheckResult::warn(
                "declared_disks",
                format!("could not load pool membership: {e}"),
            );
        }
    };

    let members = pool_membership.iter_by_name();
    let classifications: Vec<(String, String, DiskState)> = members
        .into_iter()
        .map(|(uuid, member)| {
            let by_id = member.by_id.as_str().to_owned();
            let state = classify_disk_state(ctx.runner, Path::new(&by_id), uuid);
            (member.name.as_str().to_owned(), by_id, state)
        })
        .collect();

    summarize_declared_disks(&classifications)
}

fn probe_mountpoint_is_mounted<R: CommandRunner>(runner: &R, mount_point: &MountPoint) -> bool {
    matches!(
        runner.run(&CmdRequest::MountpointCheck {
            path: mount_point.clone(),
        }),
        Ok(out) if out.exit_status == 0
    )
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
    let is_mounted = probe_mountpoint_is_mounted(ctx.runner, &mount_point);
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
            ctx.df_snapshot = Some(DfSnapshot::Error(e.to_string()));
            return;
        }
    };

    match parse_btrfs_df_json(&raw) {
        Ok(df) => ctx.df_snapshot = Some(DfSnapshot::Ok(df)),
        Err(e) => ctx.df_snapshot = Some(DfSnapshot::Error(e.to_string())),
    }
}

fn ensure_pool_state<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) {
    if ctx.pool_state.is_some() {
        return;
    }

    let config = match &ctx.config {
        Some(c) => c,
        None => return,
    };

    let mount_point = config.mount_point().to_owned();

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return;
    }

    ctx.pool_state = Some(probe::probe_pool(ctx.runner, ctx.fs, &mount_point));
}

fn check_profile_mismatch<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    bg_type: BtrfsBgType,
    check_name: &str,
    type_label: &str,
) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult::skip(check_name, "skipped (config not available)");
    }

    ensure_df_snapshot(ctx);

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();
    let df_snapshot = ctx
        .df_snapshot
        .as_ref()
        .expect("ensure_df_snapshot sets df_snapshot when config is present");

    match df_snapshot {
        DfSnapshot::NotMounted => CheckResult::skip(check_name, "skipped (pool not mounted)"),
        DfSnapshot::Error(e) => CheckResult::warn(
            check_name,
            format!("could not inspect {type_label} profiles: {e}"),
        ),
        DfSnapshot::Ok(df) => {
            let entries: Vec<_> = df.entries.iter().filter(|e| e.bg_type == bg_type).collect();

            let profiles: std::collections::BTreeSet<&BtrfsProfile> =
                entries.iter().map(|e| &e.bg_profile).collect();

            if profiles.len() <= 1 {
                let profile_name = profiles
                    .into_iter()
                    .next()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into());
                CheckResult::ok(check_name, format!("{type_label} profile: {profile_name}"))
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
                let suggestion = match preflight::probe_missing_devids(ctx.runner, &mount_point) {
                    Ok(missing) if !missing.is_empty() => {
                        "pool is degraded -- replace missing device(s) first, then rebalance"
                            .to_owned()
                    }
                    _ => format!(
                        "run: btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft {mount_point}"
                    ),
                };
                CheckResult::warn(
                    check_name,
                    format!(
                        "mixed {type_label} profiles ({}); {suggestion}",
                        parts.join(", ")
                    ),
                )
            }
        }
    }
}

fn check_pool_missing_devices<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult::skip("pool_missing_devices", "skipped (config not available)");
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip("pool_missing_devices", "skipped (pool not mounted)");
    }

    let mount_point = ctx.config.as_ref().unwrap().mount_point().clone();

    match preflight::probe_missing_devids(ctx.runner, &mount_point) {
        Ok(missing) if missing.is_empty() => {
            CheckResult::ok("pool_missing_devices", "no missing devices")
        }
        Ok(missing) => {
            let devids: Vec<String> = missing.iter().map(|d| d.to_string()).collect();
            CheckResult::warn(
                "pool_missing_devices",
                format!(
                    "pool has {} missing device{} (devid{}: {}); replace with: braid replace --old <disk> --new <disk> --missing-id <devid>",
                    missing.len(),
                    if missing.len() == 1 { "" } else { "s" },
                    if missing.len() == 1 { "" } else { "s" },
                    devids.join(", "),
                ),
            )
        }
        Err(e) => CheckResult::warn(
            "pool_missing_devices",
            format!("could not probe for missing devices: {e}"),
        ),
    }
}

fn check_foreign_luks_uuid<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    const NAME: &str = "foreign_luks_uuid";
    if ctx.config.is_none() {
        return CheckResult::skip(NAME, "skipped (config not available)");
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(NAME, "skipped (pool not mounted)");
    }

    let membership = match membership::load_membership(ctx.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return CheckResult::skip(NAME, "skipped (no pool membership file)");
        }
        Err(e) => {
            return CheckResult::warn(NAME, format!("could not load pool membership: {e}"));
        }
    };

    ensure_pool_state(ctx);
    let pool = match ctx
        .pool_state
        .as_ref()
        .expect("ensure_pool_state seeds the cache when config is present and mounted")
    {
        Ok(pool) => pool,
        Err(e) => {
            return CheckResult::warn(NAME, format!("could not probe pool: {e}"));
        }
    };

    // Keep this read-only: mutating enrichment would re-emit the transient
    // per-UUID warning on every doctor run.
    let foreign = membership::foreign_luks_uuids(&membership, pool);

    if foreign.is_empty() {
        return CheckResult::ok(NAME, "no foreign LUKS UUIDs in live pool");
    }

    let n = foreign.len();
    let entries: Vec<String> = foreign
        .iter()
        .map(|(uuid, mapper)| format!("{uuid} at mapper {mapper}"))
        .collect();
    CheckResult::fail(
        NAME,
        format!(
            "{n} foreign LUKS UUID{plural} in live pool: {body} -- restore with 'btrfs device remove /dev/mapper/<mapper> {mp}' then 'cryptsetup close <mapper>'",
            plural = if n == 1 { "" } else { "s" },
            body = entries.join("; "),
            mp = ctx.config.as_ref().unwrap().mount_point(),
        ),
    )
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
/// `--json` mode suppresses the beep: machine-readable output must never
/// produce audible side effects. The check still appears in the JSON report
/// (as `Skip`) so scripts auditing doctor output can see it.
///
/// This is the public entry point. It hits the real notifier config path;
/// unit tests target `check_beep_path_inner` directly so they can inject the
/// notifier path while keeping the side-effect gates deterministic.
fn check_beep_path<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    options: DoctorOptions,
) -> CheckResult {
    check_beep_path_inner(ctx, Path::new(NOTIFIER_CONFIG_PATH), options)
}

/// UPS doctor check for `braid.ups.enable = true`.
///
/// A spawn failure or missing `upsc` is `Fail` because the enabled UPS
/// configuration cannot verify its load-bearing shutdown path. `upsc`
/// non-zero output, including an unreachable upsd daemon or unknown UPS name,
/// stays `Warn`: the operator can fix NUT state directly, and braid does not
/// intervene in NUT lifecycle. Skips with a distinct reason when config is
/// unavailable; otherwise skips when UPS is not configured.
fn check_ups_daemon_up<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let name = "ups_daemon";
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult::skip(name, "skipped (config not available)");
    };
    let Some(ups_cfg) = config.ups() else {
        return CheckResult::skip(name, "skipped (braid.ups not enabled)");
    };
    match crate::ups::query_ups(ctx.runner, &ups_cfg.name) {
        Err(crate::ups::UpsQueryError::InvocationFailed(e)) => CheckResult::fail(
            name,
            format!("upsc invocation failed: {e} -- is pkgs.nut on PATH?"),
        ),
        Err(crate::ups::UpsQueryError::QueryFailed { exit_code, stderr }) => CheckResult::warn(
            name,
            format!(
                "upsc {} failed (exit {exit_code}): {stderr} -- \
                 check 'systemctl status upsd.service' or verify the UPS name",
                ups_cfg.name
            ),
        ),
        Ok(out) if out.status_flags.is_empty() => CheckResult::warn(
            name,
            format!(
                "upsc {} responded but ups.status is empty -- driver may still be starting",
                ups_cfg.name
            ),
        ),
        Ok(_) => CheckResult::ok(name, format!("upsc {} reachable", ups_cfg.name)),
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
    let raw = runner.run(&CmdRequest::SystemctlShowActiveState {
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
/// when UPS is not configured or when the pool is not mounted (no safety
/// implication then).
fn check_braid_online_active_when_mounted<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    let name = "braid_online_active";
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult::skip(name, "skipped (config not available)");
    };
    if config.ups().is_none() {
        return CheckResult::skip(name, "skipped (braid.ups not enabled)");
    }
    let mount_point = config.mount_point().clone();
    if !probe_mountpoint_is_mounted(ctx.runner, &mount_point) {
        return CheckResult::skip(
            name,
            "skipped (pool not mounted -- braid-online only matters while online)",
        );
    }
    let state = match read_braid_online_active_state(ctx.runner) {
        Ok(state) => state,
        Err(e) => {
            return CheckResult::fail(name, format!("systemctl spawn failed: {e}"));
        }
    };
    match classify_braid_online_active_state(&state) {
        BraidOnlineActiveState::OkSettled => {
            CheckResult::ok(name, format!("braid-online.service is {state}"))
        }
        BraidOnlineActiveState::Activating => CheckResult::warn(
            name,
            "braid-online.service is activating -- UPS shutdown hook is not confirmed yet; re-run braid doctor shortly",
        ),
        BraidOnlineActiveState::Fail => CheckResult::fail(
            name,
            format!(
                "braid-online.service is {state} -- UPS shutdown will not unmount the pool. \
                 Run `systemctl start braid-online.service` or re-run `braid unlock`."
            ),
        ),
    }
}

fn check_beep_path_inner<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    notifier_path: &Path,
    options: DoctorOptions,
) -> CheckResult {
    let name = "beep_path";

    // 1. Read the notifier config the NixOS module wrote. Absent -> Skip.
    //    A bare `braid` install (no monitor module imported) won't have it.
    let raw = match std::fs::read_to_string(notifier_path) {
        Ok(s) => s,
        Err(_) => {
            return CheckResult::skip(name, "skipped (braid monitor not configured)");
        }
    };

    // 2. Parse. Malformed = real defect: the module wrote junk.
    let cfg: NotifierConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::fail(name, format!("{}: malformed: {e}", notifier_path.display()));
        }
    };

    // 3. Beep disabled is a clean Skip.
    let probe_path = match cfg.beep_probe_path {
        Some(p) => p,
        None => {
            return CheckResult::skip(name, "skipped (beep monitoring disabled)");
        }
    };

    // 4. JSON mode is for programmatic consumption -- emitting an audible
    //    side effect from a data-output command is wrong. The check still
    //    appears in the report (as Skip) so scripts auditing doctor output
    //    can see it; the wrapper is simply never invoked.
    if options.json {
        return CheckResult::skip(
            name,
            "skipped in --json mode -- rerun with --beep without --json to play the alert test beep",
        );
    }

    // 5. Plain doctor confirms beep monitoring is configured without playing
    //    sound. The runner is only invoked for explicit --beep.
    if !options.beep {
        return CheckResult::skip(
            name,
            "skipped (pass --beep to play the audible alert test beep)",
        );
    }

    // 6. Run the canonical wrapper. This PLAYS the real short alert beep
    //    (1 kHz, 500 ms) -- same code path the alert service uses. Hearing
    //    the beep is both the success signal AND a preview of what real
    //    disk alerts will sound like.
    match ctx
        .runner
        .run(&CmdRequest::BraidBeepProbe { path: probe_path })
    {
        Ok(out) if out.exit_status == 0 => CheckResult::ok(
            name,
            "alert test beep command succeeded -- you should have heard a 1 kHz, 500 ms disk-alert beep",
        ),
        Ok(out) => CheckResult::fail(
            name,
            format!(
                "could not play alert test beep (braid-beep-probe exited {}) \
                 -- speaker likely broken: missing pcspkr device, evdev \
                 permissions wrong, or kmod blacklist still active: {}",
                out.exit_status,
                out.stderr.trim()
            ),
        ),
        Err(e) => CheckResult::fail(
            name,
            format!("could not play alert test beep (braid-beep-probe failed to spawn): {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn overall_status(checks: &[CheckResult]) -> CheckStatus {
    let has = |s: CheckStatus| checks.iter().any(|c| c.status == s);
    if has(CheckStatus::Fail) {
        CheckStatus::Fail
    } else if has(CheckStatus::Warn) {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    }
}

pub fn run_doctor<R: CommandRunner>(
    config_path: &Path,
    runner: &R,
    fs: &dyn Filesystem,
    paths: &StatePaths,
    options: DoctorOptions,
) -> DoctorReport {
    let mut ctx = DoctorContext {
        config_path: config_path.to_owned(),
        config_value: None,
        config: None,
        runner,
        fs,
        paths,
        mountpoint_is_mounted: None,
        df_snapshot: None,
        pool_state: None,
    };

    let checks = vec![
        check_config_file(&mut ctx),
        check_config_schema(&mut ctx),
        check_config_permissions(&mut ctx),
        check_declared_disks(&mut ctx),
        check_pool_missing_devices(&mut ctx),
        check_foreign_luks_uuid(&mut ctx),
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
            "foreign_luks_uuid" => "foreign uuids",
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
    let fs = RealFilesystem;
    let report = run_doctor(config_path, &runner, &fs, paths, options);

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
// Test-only constructors
//
// The fixture module `crate::test_fixtures::doctor` cannot field-construct
// `DoctorContext` directly because its fields stay module-private (and
// `DoctorContext::df_snapshot` references the module-private `DfSnapshot`).
// These `#[cfg(test)] pub(crate)` constructors keep production-side internals
// encapsulated while letting fixture code build the same shapes.
// ---------------------------------------------------------------------------

#[cfg(test)]
static REAL_FILESYSTEM_FOR_TESTS: RealFilesystem = RealFilesystem;

#[cfg(test)]
impl<'a, R: CommandRunner> DoctorContext<'a, R> {
    pub(crate) fn for_test_parsed(runner: &'a R, paths: &'a StatePaths, config_json: &str) -> Self {
        Self::for_test_parsed_with_fs(runner, &REAL_FILESYSTEM_FOR_TESTS, paths, config_json)
    }

    pub(crate) fn for_test_parsed_with_fs(
        runner: &'a R,
        fs: &'a dyn Filesystem,
        paths: &'a StatePaths,
        config_json: &str,
    ) -> Self {
        let value: serde_json::Value =
            serde_json::from_str(config_json).expect("test config JSON parses");
        let config: Config = serde_json::from_value(value.clone()).expect("test config parses");
        Self {
            config_path: PathBuf::new(),
            config_value: Some(value),
            config: Some(config),
            runner,
            fs,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
            pool_state: None,
        }
    }

    pub(crate) fn for_test_beep(runner: &'a R, paths: &'a StatePaths) -> Self {
        Self {
            config_path: PathBuf::new(),
            config_value: None,
            config: None,
            runner,
            fs: &REAL_FILESYSTEM_FOR_TESTS,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
            pool_state: None,
        }
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
    use crate::test_fixtures::{
        DF_MIXED, DF_MIXED_METADATA, DF_RAID1_CLEAN, DfQueryFailureRunner, DoctorMockFs,
        PoolMissingDevicesRunner, UpscSpawnFailureRunner, beep_ctx, cls, config_with_ups_enabled,
        config_without_ups, device_usage_healthy, device_usage_with_missing, df_json, df_json_fail,
        disk_member_with, human_options, is_luks_ok, isolated_paths, luks_dump_text_ok,
        luks_uuid_ok, mountpoint_fail, mountpoint_ok, parsed_doctor_ctx,
        systemctl_show_active_state_output, test_uuid, ups_ctx, valid_config_json, write_temp,
    };
    use crate::types::{MapperName, MountPoint};

    fn find_check<'a>(report: &'a DoctorReport, name: &str) -> &'a CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    fn doctor_btrfs_show(devices: &[(&str, u64)]) -> RawCommandOutput {
        let mut body = format!(
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices {} FS bytes used 1.00GiB\n",
            devices.len()
        );
        for (mapper, devid) in devices {
            body.push_str(&format!(
                "\tdevid {devid:>4} size 10.00GiB used 2.00GiB path /dev/mapper/{mapper}\n"
            ));
        }
        RawCommandOutput {
            cmd: "btrfs filesystem show /mnt/storage".into(),
            stdout: body,
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn doctor_cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tcipher:  aes-xts-plain64\n\
                 \tdevice:  {device}\n\
                 \tsector size:  512\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn doctor_cryptsetup_uuid_ok(device: &str, uuid: &LuksUuid) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup luksUUID {device}"),
            stdout: format!("{uuid}\n"),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn save_doctor_membership(paths: &StatePaths, entries: &[(u64, &str, &str, Option<u64>)]) {
        let mut m = membership::PoolMembership::empty();
        for (seed, name, by_id, devid) in entries {
            let (uuid, member) = disk_member_with(*seed, name, by_id, *devid, None);
            m.insert(uuid, member).expect("fixture member inserts");
        }
        membership::save_membership(&m, paths).expect("fixture membership saves");
    }

    fn foreign_luks_uuid_runner(
        pool_devices: Vec<(&'static str, u64, &'static str, LuksUuid)>,
    ) -> MockRunner {
        let mut runner = MockRunner::default();
        let (mp_req, mp_out) = mountpoint_ok();
        runner = runner.with_output(mp_req, mp_out);

        let show_devices: Vec<(&str, u64)> = pool_devices
            .iter()
            .map(|(mapper, devid, _, _)| (*mapper, *devid))
            .collect();
        runner = runner.with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            doctor_btrfs_show(&show_devices),
        );

        for (mapper, _, device, uuid) in pool_devices {
            runner = runner
                .with_output(
                    CmdRequest::CryptsetupStatus {
                        mapper: MapperName(mapper.to_owned()),
                    },
                    doctor_cryptsetup_status_active(mapper, device),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: device.to_owned(),
                    },
                    doctor_cryptsetup_uuid_ok(device, &uuid),
                );
        }

        runner
    }

    // Intent: a syntactically valid Config parses + schema-validates, and
    //   declared_disks skips when no pool.json membership file exists.
    // Why it exists: pins the post-ADR-017 contract that declared_disks
    //   sources membership from pool.json (not config.json), so a valid
    //   config without pool.json yields Skip -- not an error, not Warn.
    // Scenario: NixOS-generated config.json reaches a host that has not
    //   yet run `braid add`; doctor reports config OK and declared_disks
    //   Skip in the same run.
    #[test]
    fn valid_config_parses_ok_declared_disks_skips() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        assert_eq!(report.checks.len(), 11);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        // declared_disks skips since no pool membership file exists in test env
        assert_eq!(
            find_check(&report, "declared_disks").status,
            CheckStatus::Skip
        );
        // beep_path is intentionally not asserted here: it depends on real host
        // state (/etc/braid/notifier-config.json). Deterministic coverage
        // lives in the check_beep_path_inner tests.
    }

    /* Intent: valid custom config files skip canonical permission enforcement.
     * Why it exists: `braid doctor --config /tmp/...` is commonly used for
     * diagnostics and should still validate file presence and schema without
     * warning about debug-file ownership or mode bits.
     * Scenario: an operator runs doctor against a temporary copy of the
     * generated config while investigating an unrelated issue.
     */
    #[test]
    fn valid_custom_config_skips_permissions() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );

        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Skip);
        assert_eq!(perm.message, "skipped (custom config path)");
    }

    /* Intent: canonical permission enforcement uses exact path text.
     * Why it exists: `Path` equality can treat a dotted path as equivalent to
     * the default path, but the product rule intentionally skips anything that
     * is not exactly `/etc/braid/config.json`.
     * Scenario: an operator passes `--config /etc/braid/./config.json` while
     * debugging and expects it to behave like any other custom path.
     */
    #[test]
    fn dotted_default_path_skips_permissions_lexically() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        ctx.config_path = PathBuf::from("/etc/braid/./config.json");

        let perm = check_config_permissions(&mut ctx);

        assert_eq!(perm.status, CheckStatus::Skip);
        assert_eq!(perm.message, "skipped (custom config path)");
    }

    #[test]
    fn missing_file_fail_skip() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &MockRunner::default(),
            &RealFilesystem,
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
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
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
    fn valid_json_with_extra_fields_parses_ok() {
        // Config no longer has disks -- extra fields are ignored.
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
    }

    // Intent: empty mount_point fails Config schema validation, with the
    //   "must not be empty" message surfaced to the doctor report.
    // Why it exists: pins the user-facing failure mode for the most common
    //   hand-edit mistake (blanking mount_point) so the doctor report
    //   says exactly what is wrong.
    // Scenario: an operator hand-edits config.json and leaves mount_point
    //   as the empty string; doctor must Fail config_schema and include
    //   the schema-builder error message.
    #[test]
    fn valid_json_bad_schema_empty_mount() {
        let f = write_temp(r#"{"mount_point":""}"#);
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        let schema = find_check(&report, "config_schema");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(
            schema.message.contains("mount_point must not be empty"),
            "unexpected message: {}",
            schema.message
        );
    }

    /* Intent: run_doctor distinguishes schema-invalid config from absent UPS.
     * Why it exists: `check_config_schema` only populates ctx.config after full
     * deserialization succeeds; later UPS checks must report that the config is
     * unavailable instead of implying `braid.ups` is absent.
     * Scenario: hand-edited config JSON sets an `ups` block but leaves
     * `mount_point` empty, so JSON parsing succeeds and schema validation fails.
     */
    #[test]
    fn valid_json_bad_schema_skips_ups_as_config_unavailable() {
        let f = write_temp(
            r#"{
                "mount_point": "",
                "ups": { "name": "ups" }
            }"#,
        );
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );

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

    // Intent: ups_ctx panics loudly when config JSON parses as Value but
    //   fails Config schema validation.
    // Why it exists: ups_ctx once built ctx.config = None on schema failure,
    //   letting mistyped fixtures silently flip UPS tests to the
    //   "config unavailable" skip branch.
    // Scenario: a future test-only builder reintroduces silent-drop semantics
    //   on the ups_ctx -> for_test_parsed path.
    #[test]
    #[should_panic(expected = "test config parses")]
    fn ups_ctx_panics_on_schema_invalid_config() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let _ctx = ups_ctx(
            &runner,
            &paths,
            r#"{"mount_point":"","ups":{"name":"ups"}}"#,
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

        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Skip,
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
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
            &MockRunner::default(),
            &RealFilesystem,
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

    /* Intent: raw canonical permission inspection reports unsafe write bits.
     * Why it exists: custom-path gating in run_doctor must not remove coverage
     * for the mode checks that still protect /etc/braid/config.json.
     * Scenario: the canonical config is accidentally made writable by group
     * or other users on a deployed NAS.
     */
    #[test]
    fn permissions_world_writable_warns() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let perm = check_config_permissions_for_path(f.path());
        assert_eq!(perm.status, CheckStatus::Warn);
        assert!(perm.message.contains("world-writable"), "{}", perm.message);
        assert!(perm.message.contains("group-writable"), "{}", perm.message);
    }

    /* Intent: raw canonical permission inspection accepts restrictive modes.
     * Why it exists: the helper should report only actual write-bit problems,
     * with uid ownership checked independently by the host running the test.
     * Scenario: the canonical config is a root-owned, non-writable-by-others
     * file generated by the NixOS module.
     */
    #[test]
    fn permissions_restrictive_ok() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_temp(valid_config_json());
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let perm = check_config_permissions_for_path(f.path());
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
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let perm = find_check(&report, "config_permissions");
        assert_eq!(perm.status, CheckStatus::Skip);
    }

    #[test]
    fn human_format_contains_perms_label() {
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn declared_disks_skip_when_no_config() {
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // Intent: declared_disks skips with the "no pool membership file"
    //   message even when Config schema validation fails in the same
    //   doctor run.
    // Why it exists: pins that declared_disks is decoupled from Config
    //   validity. The check reads pool.json directly (ADR 017 / ADR 024),
    //   so a Config schema failure does not change its outcome -- it does
    //   not turn the check into Fail or Warn, and the skip reason is
    //   the absent membership file, not the broken Config.
    // Scenario: an operator hand-edits config.json and leaves mount_point
    //   empty on a host without pool.json; doctor reports config_schema
    //   Fail and declared_disks Skip with "skipped (no pool membership
    //   file)" in the same run.
    #[test]
    fn declared_disks_skips_when_no_membership_even_if_config_schema_fails() {
        let f = write_temp(r#"{"mount_point":""}"#);
        let (_dir, paths) = isolated_paths();
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Fail
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (no pool membership file)");
    }

    // --- summarize_declared_disks: pure rendering tests ---
    //
    // These tests target the pure summarizer directly, building DiskState
    // classifications by hand. They never touch the filesystem, the runner,
    // or StatePaths — by design, since the impure classifier is exercised by
    // the VM test in tests/cli/braid-doctor.py.

    #[test]
    fn summarize_ok_when_all_headers_intact() {
        /*
         * Intent: when every declared disk passes both LUKS probes, the check
         *   returns Ok with the healthy declared-disks summary message.
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
        assert_eq!(result.message, "all 2 declared disks present");
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

    // Intent: a valid LUKS header whose live UUID differs from pool.json is
    //   classified as an identity mismatch, not as healthy.
    // Why it exists: doctor is the read-only early-warning surface for swapped,
    //   cloned, or reformatted disks.
    // Scenario: a by-id slot still points to a LUKS2 volume, but it is no
    //   longer the UUID-keyed member recorded in pool membership.
    #[test]
    fn classify_luks_identity_returns_luks_uuid_mismatch_when_observed_diverges() {
        let device = "/dev/disk/by-id/wwn-0x1";
        let expected = test_uuid(1);
        let observed = test_uuid(2);
        let (is_luks_req, is_luks_out) = is_luks_ok(device);
        let (dump_req, dump_out) = luks_dump_text_ok(device);
        let (uuid_req, uuid_out) = luks_uuid_ok(device, observed.as_str());
        let runner = MockRunner::default()
            .with_output(is_luks_req, is_luks_out)
            .with_output(dump_req, dump_out)
            .with_output(uuid_req, uuid_out);

        let state = classify_luks_identity(&runner, device, &expected);

        match state {
            DiskState::LuksUuidMismatch {
                expected: got_expected,
                observed: got_observed,
            } => {
                assert_eq!(got_expected.as_str(), expected.as_str());
                assert_eq!(got_observed.as_str(), observed.as_str());
            }
            other => panic!("expected LuksUuidMismatch, got {other:?}"),
        }
    }

    // Intent: a valid LUKS header whose live UUID matches pool.json keeps the
    //   existing healthy classification.
    // Why it exists: adding the UUID probe must not turn healthy declared disks
    //   into warnings or failures.
    // Scenario: a normal offline member disk is attached at its expected by-id
    //   path and still carries the journaled UUID.
    #[test]
    fn classify_luks_identity_returns_luks_header_ok_when_uuid_matches() {
        let device = "/dev/disk/by-id/wwn-0x1";
        let expected = test_uuid(1);
        let (is_luks_req, is_luks_out) = is_luks_ok(device);
        let (dump_req, dump_out) = luks_dump_text_ok(device);
        let (uuid_req, uuid_out) = luks_uuid_ok(device, expected.as_str());
        let runner = MockRunner::default()
            .with_output(is_luks_req, is_luks_out)
            .with_output(dump_req, dump_out)
            .with_output(uuid_req, uuid_out);

        let state = classify_luks_identity(&runner, device, &expected);

        match state {
            DiskState::LuksHeaderOk => {}
            other => panic!("expected LuksHeaderOk, got {other:?}"),
        }
    }

    // Intent: a LUKS UUID mismatch makes the declared_disks check fail and
    //   renders both sides of the identity comparison.
    // Why it exists: a swapped disk will be rejected by later mutating commands,
    //   so read-only doctor must fail closed first.
    // Scenario: disk1 was reformatted in place while disk2 remains the expected
    //   pool member.
    #[test]
    fn summarize_declared_disks_promotes_to_fail_on_uuid_mismatch() {
        let expected = test_uuid(1);
        let observed = test_uuid(2);
        let inputs = [
            cls(
                "disk1",
                "/dev/disk/by-id/wwn-0x1",
                DiskState::LuksUuidMismatch {
                    expected: expected.clone(),
                    observed: observed.clone(),
                },
            ),
            cls("disk2", "/dev/disk/by-id/wwn-0x2", DiskState::LuksHeaderOk),
        ];

        let result = summarize_declared_disks(&inputs);

        assert_eq!(result.status, CheckStatus::Fail);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains(&format!("expected {expected}")),
            "missing expected UUID: {msg}"
        );
        assert!(
            msg.contains(&format!("observed {observed}")),
            "missing observed UUID: {msg}"
        );
        assert!(
            msg.contains("detach the foreign disk"),
            "missing foreign-disk guidance: {msg}"
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
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let human = format_doctor_human(&report);
        assert!(
            human.contains("declared disks"),
            "expected 'declared disks':\n{human}"
        );
    }

    // --- data_profile_mismatch tests ---

    #[test]
    fn data_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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

    // Intent: data_profile_mismatch routes to replace-first language on a degraded pool.
    // Why it exists: braid's invariant is replace/repair first, then run the soft
    //   RAID1 balance to drain single-profile chunks written during degraded
    //   operation (docs/principles.md:21; tests/repro/degraded-soft-balance.py).
    //   The mixed-profile warning's balance suggestion contradicts that order on a
    //   degraded pool; this test pins the routing that keeps the two messages aligned.
    // Scenario: a 2-disk RAID1 lost a disk; new chunks were allocated as `single`
    //   while degraded. doctor reports the mixed profile and must tell the operator
    //   to replace before balancing.
    #[test]
    fn data_profile_mismatch_recommends_replace_when_degraded() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED);
        let (du_req, du_out) = device_usage_with_missing();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("mixed"), "{}", check.message);
        assert!(
            check.message.contains("degraded"),
            "expected degraded language: {}",
            check.message,
        );
        assert!(
            check.message.contains("replace"),
            "expected replace recommendation: {}",
            check.message,
        );
        assert!(
            !check.message.contains("btrfs balance"),
            "must not recommend balance on degraded pool: {}",
            check.message,
        );
    }

    // Intent: a mixed profile on a healthy pool still recommends the soft RAID1 balance.
    // Why it exists: pins the Ok(empty) probe branch. Without this, the new
    //   routing logic could regress into always emitting the degraded message,
    //   and the existing `data_profile_mixed_warns` would not catch it because
    //   that test exercises the Err fallback by leaving BtrfsDeviceUsageRaw unmocked.
    // Scenario: operator interrupted a balance midway; mixed profiles exist but
    //   all members are present. doctor should still recommend the balance.
    #[test]
    fn data_profile_mismatch_recommends_balance_when_healthy() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED);
        let (du_req, du_out) = device_usage_healthy();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("mixed"), "{}", check.message);
        assert!(
            check.message.contains("-dconvert=raid1,soft"),
            "expected soft balance suggestion on healthy pool: {}",
            check.message,
        );
        assert!(
            !check.message.contains("degraded"),
            "healthy pool must not be labeled degraded: {}",
            check.message,
        );
        assert!(
            !check.message.contains("replace"),
            "healthy pool must not recommend replace: {}",
            check.message,
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn data_profile_skip_when_config_unavailable() {
        let report = run_doctor(
            Path::new("/tmp/nonexistent-braid-doctor-test.json"),
            &MockRunner::default(),
            &RealFilesystem,
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
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    #[test]
    fn data_profile_warn_when_df_fails() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not"),
            "expected error message: {}",
            check.message
        );
    }

    /* Intent: both profile checks report a df runner error as a query warning.
     * Why it exists: `ensure_df_snapshot` caches df command errors for both
     * consumers, so the second profile check must not reinterpret the shared
     * snapshot as unavailable config or a parse failure.
     * Scenario: `btrfs filesystem df --format json` cannot be spawned or
     * queried while the pool mountpoint itself is still present.
     */
    #[test]
    fn profile_checks_warn_when_df_query_errors() {
        let runner = DfQueryFailureRunner;
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );

        let data = find_check(&report, "data_profile_mismatch");
        assert_eq!(data.status, CheckStatus::Warn);
        assert!(
            data.message.contains("could not inspect data profiles"),
            "expected data inspect warning: {}",
            data.message
        );

        let metadata = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(metadata.status, CheckStatus::Warn);
        assert!(
            metadata
                .message
                .contains("could not inspect metadata profiles"),
            "expected metadata inspect warning: {}",
            metadata.message
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "data_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not inspect data profiles"),
            "expected inspect warning: {}",
            check.message
        );
    }

    /* Intent: both profile checks report malformed df JSON as a parse warning.
     * Why it exists: `ensure_df_snapshot` stores a shared parse failure, and each
     * profile check must label that same failure with its own profile type.
     * Scenario: btrfs exits 0, but an upstream JSON schema change makes the df
     * output unreadable to braid's parser.
     */
    #[test]
    fn profile_checks_warn_when_df_json_malformed() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json("{not json");
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );

        let data = find_check(&report, "data_profile_mismatch");
        assert_eq!(data.status, CheckStatus::Warn);
        assert!(
            data.message.contains("could not inspect data profiles"),
            "expected data inspect warning: {}",
            data.message
        );

        let metadata = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(metadata.status, CheckStatus::Warn);
        assert!(
            metadata
                .message
                .contains("could not inspect metadata profiles"),
            "expected metadata inspect warning: {}",
            metadata.message
        );
    }

    // --- metadata_profile_mismatch tests ---

    // Intent: Verify metadata_profile_mismatch reports Ok for uniform RAID1 metadata.
    // Why: Ensures the check doesn't false-positive on a healthy pool.
    // Scenario: A clean 2-disk RAID1 pool has all metadata block groups as RAID1.
    #[test]
    fn metadata_profile_clean_raid1_ok() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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

    // Intent: metadata_profile_mismatch routes to replace-first language on a degraded pool.
    // Why it exists: metadata mismatch on a degraded pool follows the same
    //   replace-first invariant; this test pins the parallel routing.
    // Scenario: a 2-disk RAID1 lost a disk; new chunks were allocated as `single`
    //   while degraded. doctor reports the mixed profile and must tell the operator
    //   to replace before balancing.
    #[test]
    fn metadata_profile_mismatch_recommends_replace_when_degraded() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_MIXED_METADATA);
        let (du_req, du_out) = device_usage_with_missing();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "metadata_profile_mismatch");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("mixed"), "{}", check.message);
        assert!(
            check.message.contains("degraded"),
            "expected degraded language: {}",
            check.message,
        );
        assert!(
            check.message.contains("replace"),
            "expected replace recommendation: {}",
            check.message,
        );
        assert!(
            !check.message.contains("btrfs balance"),
            "must not recommend balance on degraded pool: {}",
            check.message,
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
            &MockRunner::default(),
            &RealFilesystem,
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
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let human = format_doctor_human(&report);
        assert!(
            human.contains("meta profiles"),
            "expected 'meta profiles':\n{human}"
        );
    }

    // --- pool_missing_devices tests ---

    // Intent: pool_missing_devices reports Ok when no devices are missing.
    // Why: ensures the check doesn't false-positive on a healthy pool.
    // Scenario: healthy 1-disk pool, all present.
    #[test]
    fn pool_missing_devices_ok_when_healthy() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
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
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // Intent: foreign_luks_uuid fails when the mounted btrfs pool contains a
    //   live LUKS UUID absent from pool.json membership.
    // Why it exists: status points operators at doctor for this diagnosis, so
    //   doctor must persistently surface the foreign live mapper.
    // Scenario: an operator force-adds an independently formatted LUKS mapper
    //   into the live pool outside braid.
    #[test]
    fn check_foreign_luks_uuid_fails_when_pool_has_unknown_uuid() {
        let (_dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[(170, "disk1", "/dev/disk/by-id/virtio-disk1", Some(1))],
        );
        let known_uuid = test_uuid(170);
        let foreign_uuid = test_uuid(171);
        let runner = foreign_luks_uuid_runner(vec![
            ("braid-disk1", 1, "/dev/vdb", known_uuid),
            ("braid-stranger", 2, "/dev/vdc", foreign_uuid.clone()),
        ]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &paths, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(report.status, CheckStatus::Fail);
        for needle in ["foreign LUKS UUID", foreign_uuid.as_str(), "braid-stranger"] {
            assert!(
                check.message.contains(needle),
                "missing {needle:?} in: {}",
                check.message
            );
        }
        let remove_pos = check
            .message
            .find("btrfs device remove")
            .expect("message must name btrfs removal first");
        let close_pos = check
            .message
            .find("cryptsetup close")
            .expect("message must name cryptsetup close");
        assert!(
            remove_pos < close_pos,
            "remediation must remove from btrfs before closing mapper: {}",
            check.message
        );
    }

    // Intent: foreign_luks_uuid reports Ok when every live pool UUID is
    //   admitted by pool.json membership.
    // Why it exists: a healthy mounted pool must not be flagged just because
    //   doctor probes the live btrfs topology.
    // Scenario: a normal one-disk pool is mounted and pool.json contains that
    //   member's LUKS UUID.
    #[test]
    fn check_foreign_luks_uuid_ok_when_membership_admits_all_uuids() {
        let (_dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[(172, "disk1", "/dev/disk/by-id/virtio-disk1", Some(1))],
        );
        let known_uuid = test_uuid(172);
        let runner = foreign_luks_uuid_runner(vec![("braid-disk1", 1, "/dev/vdb", known_uuid)]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &paths, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("no foreign LUKS UUIDs"),
            "unexpected message: {}",
            check.message
        );
    }

    // Intent: foreign_luks_uuid skips before probing when the pool is not
    //   mounted.
    // Why it exists: there is no live PoolState to compare while the NAS pool
    //   is locked or offline.
    // Scenario: an operator runs doctor before `braid unlock`.
    #[test]
    fn check_foreign_luks_uuid_skips_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let fs = DoctorMockFs::empty();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (pool not mounted)");
    }

    // Intent: foreign_luks_uuid skips when pool.json has not been created.
    // Why it exists: first-time setup should not warn about foreign UUIDs until
    //   braid has authoritative membership to compare against.
    // Scenario: the mountpoint is present but no braid add/discover write has
    //   created /var/lib/braid/pool.json yet.
    #[test]
    fn check_foreign_luks_uuid_skips_when_membership_missing() {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (no pool membership file)");
    }

    // Intent: human format includes the "missing devs" label.
    // Why: ensures the new check has a human-readable label.
    // Scenario: operator reads braid doctor output.
    #[test]
    fn human_format_contains_missing_devs_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let (du_req, du_out) = device_usage_healthy();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(du_req, du_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        let human = format_doctor_human(&report);
        assert!(
            human.contains("missing devs"),
            "expected 'missing devs':\n{human}"
        );
    }

    // -----------------------------------------------------------------------
    // check_beep_path_inner -- deterministic branch coverage
    //
    // All beep_path tests target the inner helper directly to inject the
    // notifier-config path. The runner is mocked via MockRunner::with_output
    // for the success/failure branches.
    // -----------------------------------------------------------------------

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
        let runner = MockRunner::default();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            Path::new("/tmp/nonexistent-braid-notifier-config-doctor-test.json"),
            DoctorOptions {
                json: false, // irrelevant since the file doesn't exist
                beep: false, // irrelevant since the file doesn't exist
            },
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
        let runner = MockRunner::default();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: false,
                beep: false,
            },
        );
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
        let runner = MockRunner::default();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: false,
                beep: false,
            },
        );
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.contains("beep monitoring disabled"),
            "unexpected: {}",
            result.message
        );
    }

    // Intent: when invoked in --json mode, the check skips with a clear
    //   "json mode" message AND does not invoke the runner at all, even
    //   when --beep is set and a real-looking probe path is configured.
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
        let runner = MockRunner::default();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: true,
                beep: true, // --json still suppresses it
            },
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
        let runner = MockRunner::default();
        let mut ctx = beep_ctx(&runner, &paths);
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: false,
                beep: false,
            },
        );
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
        let runner = MockRunner::default().with_output(
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
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: false,
                beep: true,
            },
        );
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
        let runner = MockRunner::default().with_output(
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
        let result = check_beep_path_inner(
            &mut ctx,
            f.path(),
            DoctorOptions {
                json: false,
                beep: true,
            },
        );
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

    // Intent: check_ups_daemon_up warns when upsc exits 0 but omits
    // ups.status.
    // Why it exists: reachable telemetry without UPS status is not a
    // query failure, but doctor must still flag that preflight cannot
    // trust the UPS state.
    // Scenario: dummy-ups has published battery data before the driver
    // has populated the status line.
    #[test]
    fn ups_daemon_check_warns_when_status_is_empty() {
        let runner = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: "battery.charge: 44\nups.load: 12\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Warn, "got: {r:?}");
        assert!(
            r.message.contains("ups.status is empty"),
            "got: {}",
            r.message
        );
    }

    // Intent: check_ups_daemon_up warns when `upsc` exits non-zero.
    // Why: query failure deserves a visible but non-fatal nudge -- the
    // operator fixes NUT state, braid does not intervene. Regression here
    // would turn every rebooting upsd.service into a false Fail that masks
    // real problems.
    // Scenario: `systemctl stop upsd.service` while doctor runs.
    #[test]
    fn ups_daemon_check_warns_when_upsc_query_fails() {
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
        assert!(r.message.contains("failed (exit 1)"), "got: {}", r.message);
        assert!(
            r.message.contains("Error: Connection refused"),
            "got: {}",
            r.message
        );
        assert!(
            r.message.contains("systemctl status upsd.service"),
            "got: {}",
            r.message
        );
        assert!(
            r.message.contains("verify the UPS name"),
            "got: {}",
            r.message
        );
    }

    // Intent: check_ups_daemon_up fails when `upsc` cannot be invoked.
    // Why: an enabled UPS configuration whose client binary is missing
    // cannot verify the load-bearing shutdown safety path; unlike a
    // temporarily unreachable daemon, this is a packaging/wrapper fault.
    // Scenario: `braid.ups.enable = true` but the braid wrapper does not
    // place NUT's `upsc` binary on PATH.
    #[test]
    fn ups_daemon_check_fails_when_upsc_spawn_fails() {
        let runner = UpscSpawnFailureRunner;
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_ups_daemon_up(&mut ctx);
        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(
            r.message.contains("upsc invocation failed"),
            "unexpected message: {}",
            r.message
        );
        assert!(
            !r.message.contains("systemctl status upsd.service"),
            "invocation failures should not point at upsd: {}",
            r.message
        );
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
                CmdRequest::SystemctlShowActiveState {
                    unit: "braid-online.service".into(),
                },
                systemctl_show_active_state_output("inactive"),
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        let r = check_braid_online_active_when_mounted(&mut ctx);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("inactive"));
        assert!(r.message.contains("UPS shutdown"));
    }

    /* Intent: check_braid_online_active_when_mounted does NOT trust
     * ctx.mountpoint_is_mounted; it re-probes mount state every call.
     * Why it exists: per ADR 020, "pool mounted but braid-online inactive"
     * is the highest-severity doctor finding -- the UPS shutdown safety
     * guarantee fails silently if we miss it. Earlier checks in run_doctor
     * may have cached `Some(false)` while the pool was still locking; if
     * the pool then comes online before the UPS check runs, a cache-trusting
     * implementation would skip with "(pool not mounted)" instead of failing.
     * Scenario: a stale cache from a previous check says unmounted; the live
     * mount probe says mounted; braid-online.service is inactive.
     */
    #[test]
    fn braid_online_check_reprobes_when_cache_is_stale() {
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
                CmdRequest::SystemctlShowActiveState {
                    unit: "braid-online.service".into(),
                },
                systemctl_show_active_state_output("inactive"),
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());
        ctx.mountpoint_is_mounted = Some(false); // stale cache from earlier check

        let r = check_braid_online_active_when_mounted(&mut ctx);

        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(r.message.contains("UPS shutdown"), "{}", r.message);
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
                CmdRequest::SystemctlShowActiveState {
                    unit: "braid-online.service".into(),
                },
                systemctl_show_active_state_output("active"),
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
                    CmdRequest::SystemctlShowActiveState {
                        unit: "braid-online.service".into(),
                    },
                    systemctl_show_active_state_output(status),
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
                CmdRequest::SystemctlShowActiveState {
                    unit: "braid-online.service".into(),
                },
                systemctl_show_active_state_output("activating"),
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
                    CmdRequest::SystemctlShowActiveState {
                        unit: "braid-online.service".into(),
                    },
                    systemctl_show_active_state_output(status),
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
}
