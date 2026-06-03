use std::collections::HashSet;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Path written by `modules/braid/monitor.nix` (`environment.etc."braid/notifier-config.json"`).
/// `check_beep_path` reads it to discover the canonical beep wrapper.
const NOTIFIER_CONFIG_PATH: &str = "/etc/braid/notifier-config.json";

/// Schema of `/etc/braid/notifier-config.json`. Tracked in lockstep with the
/// `builtins.toJSON` writer in `modules/braid/monitor.nix`. A schema change
/// must update both sides -- deserialize errors here (including unknown
/// fields) are loud (Fail), so a stale parser cannot silently degrade.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifierConfig {
    beep_probe_path: Option<String>,
}

use crate::capacity;
use crate::cmd::{CmdRequest, CommandRunner, RealRunner};
use crate::config::{Config, DEFAULT_CONFIG_PATH};
use crate::luks;
use crate::membership;
use crate::mountpoint_guard::{GuardError, MountpointGuard, RealMountpointGuard};
use crate::online_state::{BRAID_ONLINE_UNIT, OnlineStateOps, RealOnlineStateOps, UnitActiveState};
use crate::parse::smartctl::selftest_age_hours;
use crate::parse::types::{
    BtrfsBgType, BtrfsDeviceUsageOutput, BtrfsDfOutput, BtrfsProfile, SelftestSummary,
};
use crate::parse::{
    parse_btrfs_device_usage, parse_btrfs_df_json, parse_cryptsetup_luks_uuid,
    parse_smartctl_selftest_log,
};
use crate::probe::{self, Filesystem, ProbeError, RealFilesystem};
use crate::repair_hint;
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, format_bytes, get_balance_report, paused_balance_advice};
use crate::status_tag::{StatusTag, color_enabled_for_stdout, status_line};
use crate::types::{LuksUuid, PoolState};
use crate::wol::{WolReadiness, classify_wol};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl CheckResult {
    /// Sole field-initialization site so adding a result field touches one constructor.
    fn new(name: impl Into<String>, status: CheckStatus, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            message: message.into(),
            subject: None,
        }
    }

    /// Subject-tagged result used when one logical check reports per-device state.
    fn new_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        status: CheckStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            subject: Some(subject.into()),
            ..Self::new(name, status, message)
        }
    }

    fn ok(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Ok, message)
    }

    fn warn(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Warn, message)
    }

    fn fail(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Fail, message)
    }

    fn skip(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Skip, message)
    }

    fn ok_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Ok, message)
    }

    fn warn_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Warn, message)
    }

    fn fail_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Fail, message)
    }

    fn skip_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Skip, message)
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

/// Cached `btrfs device usage` result shared by doctor checks that reason
/// about per-device allocator headroom.
enum DeviceUsageSnapshot {
    NotMounted,
    Error(String),
    Ok(BtrfsDeviceUsageOutput),
}

/// Per-run state for `braid doctor`: caches mountpoint, df, device-usage, and
/// live-pool probes across checks so the orchestrator avoids re-querying btrfs,
/// and threads the parsed config plus runner/filesystem borrows each check needs.
pub(crate) struct DoctorContext<'a, R: CommandRunner> {
    config_path: PathBuf,
    config_value: Option<serde_json::Value>,
    config: Option<Config>,
    runner: &'a R,
    online_ops: RealOnlineStateOps<'a>,
    fs: &'a dyn Filesystem,
    paths: &'a StatePaths,
    mountpoint_is_mounted: Option<bool>,
    df_snapshot: Option<DfSnapshot>,
    device_usage: Option<DeviceUsageSnapshot>,
    pool_state: Option<Result<PoolState, ProbeError>>,
}

#[derive(Debug, Clone, Copy)]
pub struct DoctorOptions {
    pub json: bool,
    pub beep: bool,
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
/// the variants pin the rendered declared-disk outcomes (header Ok, UUID mismatch,
/// offline, header unreadable, missing, non-block, probe-failed).
#[derive(Debug, Clone)]
pub(crate) enum DiskState {
    /// Header probes succeeded and the live LUKS UUID matched the pool.json key.
    LuksHeaderOk,
    /// Present and identity-verified, but absent from the live btrfs pool.
    Offline,
    /// `cryptsetup isLuks` and `cryptsetup luksUUID` succeeded, but the live
    /// LUKS UUID does not match the pool.json key.
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
    /// `cryptsetup isLuks` exited non-zero -- crypt_load could not read or
    /// validate a LUKS header. Where genuine metadata damage lands. Severe.
    LuksHeaderUnreadable,
}

/// Live btrfs topology as `declared_disks` sees it, kept separate so a mounted
/// pool probe failure warns instead of collapsing into offline-pool behavior.
enum LiveTopology {
    /// Pool not mounted or config absent; preserve identity-only behavior.
    Offline,
    /// Pool mounted and probed; UUIDs of assembled members.
    Online(HashSet<LuksUuid>),
    /// Pool mounted but `probe_pool` failed; topology is indeterminate.
    Unavailable(String),
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

/// Cross-check a verified declared member against live btrfs membership without
/// masking stronger per-disk problems or fabricating offline rows on probe error.
fn reconcile_with_live_pool(
    uuid: &LuksUuid,
    state: DiskState,
    topology: &LiveTopology,
) -> DiskState {
    match (&state, topology) {
        (DiskState::LuksHeaderOk, LiveTopology::Online(live)) if !live.contains(uuid) => {
            DiskState::Offline
        }
        _ => state,
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
fn summarize_declared_disks(
    classifications: &[(String, String, DiskState)],
    topology_unavailable: Option<&str>,
) -> CheckResult {
    let total = classifications.len();
    let mut missing: Vec<String> = Vec::new();
    let mut not_block: Vec<String> = Vec::new();
    let mut offline: Vec<String> = Vec::new();
    let mut probe_failed: Vec<String> = Vec::new();
    let mut header_unreadable: Vec<String> = Vec::new();
    let mut uuid_mismatch: Vec<String> = Vec::new();

    for (name, by_id, state) in classifications {
        match state {
            DiskState::LuksHeaderOk => {}
            DiskState::Offline => offline.push(format!("{name} ({by_id})")),
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
        }
    }

    let disk_problem_count = missing.len()
        + not_block.len()
        + offline.len()
        + probe_failed.len()
        + header_unreadable.len()
        + uuid_mismatch.len();

    if disk_problem_count == 0 && topology_unavailable.is_none() {
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
    if !uuid_mismatch.is_empty() {
        parts.push(format!(
            "{} with LUKS UUID mismatch: {}",
            uuid_mismatch.len(),
            uuid_mismatch.join("; ")
        ));
    }
    if !offline.is_empty() {
        parts.push(format!(
            "{} present but not in the live pool: {}",
            offline.len(),
            offline.join(", ")
        ));
    }
    if !probe_failed.is_empty() {
        parts.push(format!(
            "{} with LUKS header probe failures: {}",
            probe_failed.len(),
            probe_failed.join("; ")
        ));
    }

    let message = if disk_problem_count > 0 {
        let mut message = format!(
            "{}/{} {} problems: {}",
            disk_problem_count,
            total,
            if total == 1 { "disk has" } else { "disks have" },
            parts.join("; ")
        );
        if let Some(reason) = topology_unavailable {
            message.push_str(&format!(
                "; could not compare declared disks to live pool: {reason}"
            ));
        }
        message
    } else {
        format!(
            "could not compare declared disks to live pool: {}",
            topology_unavailable.expect("non-ok path with zero disk problems implies unavailable")
        )
    };
    if uuid_mismatch.is_empty() {
        CheckResult::warn("declared_disks", message)
    } else {
        CheckResult::fail("declared_disks", message)
    }
}

/// Shared membership gate for doctor checks that need authoritative pool members.
/// Keeps missing, corrupt, and empty pool.json handling consistent across
/// independently rendered checks.
fn load_membership_or_check_result<R: CommandRunner>(
    ctx: &DoctorContext<'_, R>,
    check_name: &'static str,
) -> Result<membership::PoolMembership, CheckResult> {
    match membership::load_membership(ctx.paths) {
        Ok(m) if m.is_empty() => Err(CheckResult::skip(
            check_name,
            "skipped (no pool members declared)",
        )),
        Ok(m) => Ok(m),
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(CheckResult::skip(
                check_name,
                "skipped (no pool membership file)",
            ))
        }
        Err(e) => Err(CheckResult::warn(
            check_name,
            format!("could not load pool membership: {e}"),
        )),
    }
}

fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let pool_membership = match load_membership_or_check_result(ctx, "declared_disks") {
        Ok(m) => m,
        Err(cr) => return cr,
    };

    let topology = if ensure_mountpoint_is_mounted(ctx) == Some(true) {
        ensure_pool_state(ctx);
        match ctx
            .pool_state
            .as_ref()
            .expect("ensure_pool_state seeds the cache when config is present and mounted")
        {
            Ok(pool) if pool.mounted => {
                LiveTopology::Online(pool.devices.iter().map(|d| d.luks_uuid.clone()).collect())
            }
            Ok(_) => LiveTopology::Offline,
            Err(e) => LiveTopology::Unavailable(e.to_string()),
        }
    } else {
        LiveTopology::Offline
    };
    let topology_unavailable = match &topology {
        LiveTopology::Unavailable(reason) => Some(reason.as_str()),
        _ => None,
    };

    let members = pool_membership.iter_by_name();
    let classifications: Vec<(String, String, DiskState)> = members
        .into_iter()
        .map(|(uuid, member)| {
            let by_id = member.by_id.as_str().to_owned();
            let base = classify_disk_state(ctx.runner, Path::new(&by_id), uuid);
            let state = reconcile_with_live_pool(uuid, base, &topology);
            (member.name.as_str().to_owned(), by_id, state)
        })
        .collect();

    summarize_declared_disks(&classifications, topology_unavailable)
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
    let is_mounted = ctx
        .online_ops
        .is_mountpoint(Path::new(mount_point.as_str()))
        .unwrap_or(false);
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

/// Populate the shared per-device usage cache for allocator-headroom checks.
fn ensure_device_usage<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) {
    if ctx.device_usage.is_some() {
        return;
    }

    let config = match &ctx.config {
        Some(c) => c,
        None => return,
    };

    let mount_point = config.mount_point().to_owned();

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        ctx.device_usage = Some(DeviceUsageSnapshot::NotMounted);
        return;
    }

    let raw = match ctx.runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => {
            ctx.device_usage = Some(DeviceUsageSnapshot::Error(e.to_string()));
            return;
        }
    };

    match parse_btrfs_device_usage(&raw) {
        Ok(usage) => ctx.device_usage = Some(DeviceUsageSnapshot::Ok(usage)),
        Err(e) => {
            ctx.device_usage = Some(DeviceUsageSnapshot::Error(format!(
                "could not parse output: {e}"
            )))
        }
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
                ensure_pool_state(ctx);
                let suggestion = match ctx
                    .pool_state
                    .as_ref()
                    .expect("ensure_pool_state seeds the cache when config is present and mounted")
                {
                    Ok(pool) if !pool.missing_devids.is_empty() => {
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

    ensure_pool_state(ctx);
    match ctx
        .pool_state
        .as_ref()
        .expect("ensure_pool_state seeds the cache when config is present and mounted")
    {
        Ok(pool) if pool.missing_devids.is_empty() => {
            CheckResult::ok("pool_missing_devices", "no missing devices")
        }
        Ok(pool) => {
            let devids: Vec<String> = pool.missing_devids.iter().map(|d| d.to_string()).collect();
            let n = pool.missing_devids.len();
            let repair_command = repair_hint::missing_replace_command(None);
            let cross_check = match pool.missing_devids.as_slice() {
                [devid] => format!(
                    "Optional cross-check: `{}`.",
                    repair_hint::missing_replace_command_with_devid(None, *devid)
                ),
                _ => repair_hint::optional_missing_id_cross_check_phrase(),
            };
            let cross_check_target = if n == 1 {
                "Use the listed ID."
            } else {
                "Use one of the listed IDs."
            };
            CheckResult::warn(
                "pool_missing_devices",
                format!(
                    "pool has {} missing device{} (devid{}: {}); replace with: \
                     `{repair_command}`; {cross_check} {cross_check_target} \
                     Use `braid status` to see the missing disk's name",
                    n,
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "" } else { "s" },
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

/// Read-only RAID1 headroom check shared with status's ENOSPC risk advisory.
fn check_enospc_risk<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    const NAME: &str = "enospc_risk";
    if ctx.config.is_none() {
        return CheckResult::skip(NAME, "skipped (config not available)");
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(NAME, "skipped (pool not mounted)");
    }

    ensure_pool_state(ctx);
    let missing_count = match ctx
        .pool_state
        .as_ref()
        .expect("ensure_pool_state seeds the cache when config is present and mounted")
    {
        Err(e) => {
            return CheckResult::warn(
                NAME,
                format!("could not probe pool state -- ENOSPC risk indeterminate: {e}"),
            );
        }
        Ok(pool) if pool.missing_count > 0 => {
            return CheckResult::skip(NAME, "skipped (pool is degraded)");
        }
        Ok(pool) => pool.missing_count,
    };

    ensure_device_usage(ctx);
    let device_usage = ctx
        .device_usage
        .as_ref()
        .expect("ensure_device_usage sets device_usage when config is present");
    match device_usage {
        DeviceUsageSnapshot::NotMounted => CheckResult::skip(NAME, "skipped (pool not mounted)"),
        DeviceUsageSnapshot::Error(_) => CheckResult::warn(
            NAME,
            "btrfs device usage failed -- ENOSPC risk indeterminate",
        ),
        DeviceUsageSnapshot::Ok(usage) => {
            match capacity::enospc_risk_advisory(&usage.devices, missing_count)
                .into_iter()
                .next()
            {
                Some(advisory) => CheckResult::warn(NAME, advisory),
                None => CheckResult::ok(NAME, "per-device unallocated space healthy"),
            }
        }
    }
}

/// Mounted-pool paused-balance check that points operators at resume instead
/// of cancel/restart, preserving btrfs's existing balance progress.
fn check_paused_balance<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let name = "paused_balance";
    if ctx.config.is_none() {
        return CheckResult::skip(name, "skipped (config not available)");
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(name, "skipped (pool not mounted)");
    }

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();
    match get_balance_report(ctx.runner, &mount_point) {
        BalanceReport::Paused { .. } => {
            let advice = paused_balance_advice(&mount_point);
            CheckResult::warn(
                name,
                format!("{}; run: {}", advice.header, advice.resume_cmd),
            )
        }
        BalanceReport::Idle | BalanceReport::Running { .. } => {
            CheckResult::ok(name, "no paused balance")
        }
        BalanceReport::Unknown => CheckResult::warn(name, "could not inspect balance status"),
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

    let membership = match load_membership_or_check_result(ctx, NAME) {
        Ok(m) => m,
        Err(cr) => return cr,
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
    let mp = ctx.config.as_ref().unwrap().mount_point();
    // Pair each foreign mapper's diagnosis with its own paste-ready
    // remove+close recipe, so multi-foreign output reads as N independent
    // recoveries rather than one long sequence. Every mapper was observed
    // live, so no `<mapper>` placeholder is needed.
    let recoveries: Vec<String> = foreign
        .iter()
        .map(|(uuid, mapper)| {
            format!(
                "{uuid} at mapper {mapper} -- restore with 'btrfs device remove /dev/mapper/{mapper} {mp}' then 'cryptsetup close {mapper}'"
            )
        })
        .collect();
    CheckResult::fail(
        NAME,
        format!(
            "{n} foreign LUKS UUID{plural} in live pool: {body}",
            plural = if n == 1 { "" } else { "s" },
            body = recoveries.join("; "),
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

fn check_system_profile_mismatch<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    check_profile_mismatch(
        ctx,
        BtrfsBgType::System,
        "system_profile_mismatch",
        "system",
    )
}

/// Advisory metadata ENOSPC check that joins logical df pressure with
/// per-device allocator headroom; either signal alone is too noisy for doctor.
fn check_metadata_enospc_pressure<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    const NAME: &str = "metadata_enospc_pressure";
    if ctx.config.is_none() {
        return CheckResult::skip(NAME, "skipped (config not available)");
    }

    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(NAME, "skipped (pool not mounted)");
    }

    ensure_df_snapshot(ctx);
    let df_snapshot = ctx
        .df_snapshot
        .as_ref()
        .expect("ensure_df_snapshot sets df_snapshot when config is present");
    let (metadata_used, metadata_total) = {
        let df = match df_snapshot {
            DfSnapshot::NotMounted => return CheckResult::skip(NAME, "skipped (pool not mounted)"),
            DfSnapshot::Error(e) => {
                return CheckResult::warn(
                    NAME,
                    format!("could not inspect metadata pressure: {e}"),
                );
            }
            DfSnapshot::Ok(df) => df,
        };

        df.entries
            .iter()
            .filter(|entry| entry.bg_type == BtrfsBgType::Metadata)
            .fold((0u64, 0u64), |(used, total), entry| {
                (used + entry.bg_used, total + entry.bg_total)
            })
    };

    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();
    ensure_device_usage(ctx);
    let usage = match ctx
        .device_usage
        .as_ref()
        .expect("ensure_device_usage sets device_usage when config is present")
    {
        DeviceUsageSnapshot::NotMounted => {
            return CheckResult::skip(NAME, "skipped (pool not mounted)");
        }
        DeviceUsageSnapshot::Error(e) => {
            return CheckResult::warn(NAME, format!("could not inspect device unallocated: {e}"));
        }
        DeviceUsageSnapshot::Ok(usage) => usage,
    };
    if usage.devices.is_empty() {
        return CheckResult::warn(
            NAME,
            "could not inspect device unallocated: no devices reported",
        );
    }

    if metadata_total == 0 {
        return CheckResult::ok(NAME, "no metadata block groups yet");
    }

    let metadata_ratio = metadata_used as f64 / metadata_total as f64;

    // RAID1 metadata chunks need exactly 2 devices, not every device.
    // reference/linux/fs/btrfs/volumes.c defines RAID1 with devs_min=2,
    // devs_max=2, and ncopies=2, so a 3+ device pool only needs two
    // members with enough unallocated space for the next metadata chunk.
    let n_devices = usage.devices.len();
    let with_headroom = usage
        .devices
        .iter()
        .filter(|device| device.unallocated >= METADATA_CHUNK_HEADROOM)
        .count();

    if metadata_ratio > METADATA_PRESSURE_RATIO && with_headroom < 2 {
        // About to recommend a data balance. On a degraded pool, that widens
        // the recovery surface by allowing single-profile chunks; defer to the
        // replace-first path already surfaced by check_pool_missing_devices.
        ensure_pool_state(ctx);
        match ctx
            .pool_state
            .as_ref()
            .expect("ensure_pool_state seeds the cache when config is present and mounted")
        {
            Err(e) => {
                return CheckResult::warn(
                    NAME,
                    format!("could not probe pool state -- metadata pressure indeterminate: {e}"),
                );
            }
            Ok(pool) if pool.missing_count > 0 => {
                return CheckResult::skip(NAME, "skipped (pool is degraded)");
            }
            Ok(_) => {}
        }

        let pct = (metadata_ratio * 100.0).round() as u64;
        let headroom = format_bytes(METADATA_CHUNK_HEADROOM);
        return CheckResult::warn(
            NAME,
            format!(
                "metadata {pct}% used; only {with_headroom} of {n_devices} device(s) have >= {headroom} unallocated -- RAID1 needs 2 with headroom for the next metadata chunk; delete files to free space, or compact data with `btrfs balance start -dusage=50 {mount_point}` before metadata cannot grow."
            ),
        );
    }

    CheckResult::ok(NAME, "metadata pressure within bounds")
}

/// Metadata utilization threshold that warns before btrfs's hard-coded 80%
/// automatic chunk-allocation point in should_alloc_chunk().
const METADATA_PRESSURE_RATIO: f64 = 0.75;

/// Conservative per-device unallocated headroom for one future metadata chunk;
/// RAID1 allocation needs two devices with this much room.
const METADATA_CHUNK_HEADROOM: u64 = 1024 * 1024 * 1024;

/// SMART self-test staleness threshold in powered-on hours.
/// 90 days at 24 h/day. Matches the manual's "90 powered-on days" wording
/// and the doctor decision matrix.
const STALE_SELFTEST_THRESHOLD_HOURS: u64 = 90 * 24;

/// User-facing age phrase for SMART self-test messages.
/// Truncates to whole days and pluralises grammatically; the leading `~`
/// carries the powered-on-hour imprecision.
fn approx_days_phrase(age_hours: u64) -> String {
    let days = age_hours / 24;
    if days == 1 {
        "~1 day".to_owned()
    } else {
        format!("~{days} days")
    }
}

fn smart_selftest_hint(by_id: &str) -> String {
    format!("run: smartctl -t short {by_id}  (or -t long for full-surface scan, takes hours)")
}

fn summarize_smart_selftest(subject: &str, by_id: &str, summary: SelftestSummary) -> CheckResult {
    const NAME: &str = "smart_self_test";

    if summary.command_error {
        return CheckResult::skip_for(
            NAME,
            subject,
            "SMART self-test status unavailable (smartctl command failed)",
        );
    }

    if summary.parse_failure {
        return CheckResult::skip_for(
            NAME,
            subject,
            "SMART self-test status unavailable (smartctl JSON output not parseable)",
        );
    }

    if let Some(protocol) = summary.unsupported_protocol {
        return CheckResult::skip_for(
            NAME,
            subject,
            format!(
                "SMART self-test status unavailable ({protocol} self-test log not checked in v1)"
            ),
        );
    }

    if summary.active_errors > 0 {
        return match summary.last_failure {
            Some(failure) => CheckResult::fail_for(
                NAME,
                subject,
                format!(
                    "SMART self-test FAILED at lifetime hour {} ({}) -- investigate before further use",
                    failure.lifetime_hours, failure.kind
                ),
            ),
            None => CheckResult::fail_for(
                NAME,
                subject,
                format!(
                    "SMART self-test log reports {} active failure(s) but no failure entry was parsed -- run smartctl manually: smartctl -l selftest {by_id}",
                    summary.active_errors
                ),
            ),
        };
    }

    let Some(power_on_hours) = summary.power_on_hours else {
        return CheckResult::skip_for(
            NAME,
            subject,
            "SMART self-test status unavailable (power_on_time.hours missing -- can't measure age)",
        );
    };

    let Some(last_passing) = summary.last_passing else {
        return CheckResult::warn_for(
            NAME,
            subject,
            format!(
                "no completed SMART self-test recorded -- {}",
                smart_selftest_hint(by_id)
            ),
        );
    };

    let age_hours = selftest_age_hours(power_on_hours, last_passing.lifetime_hours);
    let age_phrase = approx_days_phrase(age_hours);
    if age_hours <= STALE_SELFTEST_THRESHOLD_HOURS {
        CheckResult::ok_for(NAME, subject, format!("passed {age_phrase} ago"))
    } else {
        CheckResult::warn_for(
            NAME,
            subject,
            format!(
                "no SMART self-test in {age_phrase} -- {}",
                smart_selftest_hint(by_id)
            ),
        )
    }
}

/// Per-drive SMART self-test doctor rows.
/// Emits one stable-name row per pool member so machine consumers key on
/// `name + subject`; emits one unscoped fallback row when pool membership
/// cannot provide any members to inspect.
fn check_smart_selftests<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> Vec<CheckResult> {
    const NAME: &str = "smart_self_test";
    let membership = match load_membership_or_check_result(ctx, NAME) {
        Ok(membership) => membership,
        Err(cr) => return vec![cr],
    };

    ensure_pool_state(ctx);
    let live = ctx.pool_state.as_ref().and_then(|r| r.as_ref().ok());

    let mut checks = Vec::new();
    for (uuid, member) in membership.iter_by_name() {
        let subject = member.name.as_str();
        let by_id = member.by_id.as_str();
        let query_device = live
            .and_then(|pool| pool.underlying_for_uuid(uuid))
            .unwrap_or(by_id);
        let raw = match ctx.runner.run(&CmdRequest::SmartctlSelftestLogJson {
            device: query_device.to_owned(),
        }) {
            Ok(raw) => raw,
            Err(e) => {
                checks.push(CheckResult::skip_for(
                    NAME,
                    subject,
                    format!(
                        "SMART self-test status unavailable (smartctl command failed to run: {e})"
                    ),
                ));
                continue;
            }
        };
        checks.push(summarize_smart_selftest(
            subject,
            by_id,
            parse_smartctl_selftest_log(&raw),
        ));
    }
    checks
}

/// Doctor check for the PC speaker alert path.
///
/// By default, validates the notifier config without playing sound. Passing
/// `--beep` plays a short alert test beep (1 kHz, 500 ms) via the canonical
/// `braid-beep-probe` wrapper -- the same code path the alert service uses.
/// A successful `--beep` run is both a notifier-health check and a positive
/// guarantee that future disk alerts will produce the same audible beep.
///
/// `--json` mode suppresses the beep as defense-in-depth: machine-readable
/// output must never produce audible side effects. The check still appears in
/// the JSON report (as `Skip`) so scripts auditing doctor output can see it.
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

/// Pure Wake-on-LAN classifier so tests can cover every ethtool output branch
/// without needing a VM NIC that supports real magic-packet wake.
fn summarize_wol(interface: &str, stdout: &str, stderr: &str, exit_status: i32) -> CheckResult {
    let name = "wake_on_lan";
    match classify_wol(stdout, stderr, exit_status) {
        WolReadiness::QueryFailed { exit, detail } => {
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            CheckResult::fail(
                name,
                format!(
                    "ethtool {interface} failed (exit {exit}){suffix} -- cannot verify Wake-on-LAN"
                ),
            )
        }
        WolReadiness::Unparseable => CheckResult::fail(
            name,
            format!(
                "could not parse ethtool output for {interface} -- expected Supports Wake-on and Wake-on lines"
            ),
        ),
        WolReadiness::Unsupported { supports } => CheckResult::fail(
            name,
            format!(
                "{interface} does not report magic-packet WoL support (Supports Wake-on: {supports}) -- use a wired NIC/driver that supports Wake-on-LAN"
            ),
        ),
        WolReadiness::Disabled { active, .. } => CheckResult::fail(
            name,
            format!(
                "{interface} supports magic-packet WoL but reports Wake-on: {active} -- rebuild, verify the interface name, and check BIOS/driver WoL settings"
            ),
        ),
        WolReadiness::Armed { active } => CheckResult::ok(
            name,
            format!("{interface} reports Wake-on: {active} (magic packet armed)"),
        ),
    }
}

/// Runtime WoL verification for auto-suspend hosts. Build-time NixOS config
/// can request magic wake, but only the live NIC state proves the NAS will not
/// suspend into an unreachable state.
fn check_wake_on_lan<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let name = "wake_on_lan";
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult::skip(name, "skipped (config not available)");
    };
    let Some(auto_suspend) = config.auto_suspend() else {
        return CheckResult::skip(name, "skipped (braid.autoSuspend not enabled)");
    };
    let interface = auto_suspend.wol_interface.as_str();
    match ctx.runner.run(&CmdRequest::EthtoolShow {
        interface: interface.to_owned(),
    }) {
        Ok(out) => summarize_wol(interface, &out.stdout, &out.stderr, out.exit_status),
        Err(e) => CheckResult::fail(
            name,
            format!(
                "ethtool invocation failed for {interface}: {e} -- is braid.packages.ethtool on PATH?"
            ),
        ),
    }
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
        Ok(q) if q.parsed.status_flags.is_empty() => CheckResult::warn(
            name,
            format!(
                "upsc {} responded but ups.status is empty -- driver may still be starting",
                ups_cfg.name
            ),
        ),
        Ok(_) => CheckResult::ok(name, format!("upsc {} reachable", ups_cfg.name)),
    }
}

/// UPS doctor check: report braid-online.service state while the pool is
/// mounted under UPS.
///
/// This is the critical configuration fault in `docs/design/decisions/020-
/// ups-integration.md`'s "braid-online becomes safety-critical"
/// section: without `braid-online.service` active, reloading, or
/// refreshing, the `SHUTDOWNCMD = systemctl poweroff` path does NOT
/// unwind `braid lock`'s ExecStop. `activating` is only a Warn because it is
/// plausibly transient, but every other non-success state is a high-severity
/// fault.
///
/// Skips with a distinct reason when config is unavailable. Otherwise skips
/// when UPS is not configured, module-managed lifecycle is not enabled, or
/// the pool is not mounted (no safety implication then). Mountpoint and
/// ActiveState probes both use the shared `OnlineStateOps` seam that dispatch
/// uses for `mark_online` and `mark_offline`.
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
    if !config.systemd_lifecycle() {
        return CheckResult::skip(
            name,
            "skipped (systemd_lifecycle not configured -- braid-online is not Rust-managed)",
        );
    }
    let mount_point = config.mount_point().clone();
    match ctx
        .online_ops
        .is_mountpoint(Path::new(mount_point.as_str()))
    {
        Ok(true) => {}
        Ok(false) => {
            return CheckResult::skip(
                name,
                "skipped (pool not mounted -- braid-online only matters while online)",
            );
        }
        Err(e) => {
            return CheckResult::fail(
                name,
                format!(
                    "mountpoint probe for {} failed: {e} -- cannot confirm UPS shutdown safety. Re-run `braid doctor`.",
                    mount_point.as_str()
                ),
            );
        }
    }
    let outcome = ctx.online_ops.unit_active_state(BRAID_ONLINE_UNIT);
    match outcome {
        Ok(
            state @ (UnitActiveState::Active
            | UnitActiveState::Reloading
            | UnitActiveState::Refreshing),
        ) => CheckResult::ok(
            name,
            format!("braid-online.service is {}", state.systemd_word()),
        ),
        Ok(UnitActiveState::Activating) => CheckResult::warn(
            name,
            "braid-online.service is activating -- UPS shutdown hook is not confirmed yet; re-run braid doctor shortly",
        ),
        Ok(
            state @ (UnitActiveState::Deactivating
            | UnitActiveState::Inactive
            | UnitActiveState::Failed
            | UnitActiveState::Maintenance),
        ) => CheckResult::fail(
            name,
            format!(
                "braid-online.service is {} -- UPS shutdown will not unmount the pool. \
                 Run `systemctl start braid-online.service` or re-run `braid unlock`.",
                state.systemd_word()
            ),
        ),
        Ok(UnitActiveState::Unknown(reason)) => CheckResult::fail(
            name,
            format!(
                "braid-online.service ActiveState unrecognised ({reason}) -- UPS shutdown will not unmount the pool. \
                 Run `systemctl start braid-online.service` or re-run `braid unlock`."
            ),
        ),
        Err(e) => CheckResult::fail(
            name,
            format!(
                "braid-online.service ActiveState read failed: {e} -- UPS shutdown will not unmount the pool. \
                 Run `systemctl start braid-online.service` or re-run `braid unlock`."
            ),
        ),
    }
}

/// Three-way reading of the mountpoint's immutable attribute. NOT a bare bool:
/// `guard.is_immutable` can legitimately fail to yield a bool (absent root,
/// unsupported fs, old kernel, I/O), and that indeterminacy must suppress a
/// finding rather than coin-flip into a false Warn or silent pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImmutabilityProbe {
    Immutable,
    Mutable,
    Indeterminate,
}

impl ImmutabilityProbe {
    /// Maps a guard read into the probe: `Ok(true) -> Immutable`,
    /// `Ok(false) -> Mutable`, any `Err` -> `Indeterminate`. The thin seam
    /// between the syscall boundary and the pure classifier.
    pub(crate) fn from_result(result: Result<bool, GuardError>) -> Self {
        match result {
            Ok(true) => Self::Immutable,
            Ok(false) => Self::Mutable,
            Err(_) => Self::Indeterminate,
        }
    }
}

/// Severity decision for the mountpoint-immutability check, returned by the pure
/// classifier so every branch is unit-testable without root or wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImmutableFinding {
    None,
    Warn(String),
    Failure(String),
}

/// Pure decision helper for the mountpoint-immutability doctor check. Both
/// inputs are tri-state because both probes (mount state and immutability) can
/// fail, and an unknown input must never produce a misleading finding.
///
/// - offline + Mutable -> Warn (invariant not yet held; self-heals next boot).
/// - online + Immutable -> Failure (a live pool root is sealed -- never happens
///   with this code; a tripwire for bugs or external interference).
/// - either input indeterminate -> None (no honest, actionable hint).
pub(crate) fn classify_mountpoint_immutability(
    mount_point: &str,
    mounted: Option<bool>,
    probe: ImmutabilityProbe,
) -> ImmutableFinding {
    match (mounted, probe) {
        // A failed mount probe could be hiding a mounted pool, so we can claim
        // neither "offline + mutable, reseal" nor "online + immutable,
        // catastrophe". Suppress both.
        (None, _) => ImmutableFinding::None,
        // Could not read the attribute (unsupported fs / old kernel / I/O). The
        // seal unit already emits the single "protection unavailable" warning;
        // a doctor hint here would be contradictory and un-actionable.
        (_, ImmutabilityProbe::Indeterminate) => ImmutableFinding::None,
        (Some(false), ImmutabilityProbe::Mutable) => ImmutableFinding::Warn(format!(
            "{mount_point} is not immutable while the pool is offline -- writes to it would land on \
             the root filesystem and be hidden when the pool mounts. It re-seals on the next boot or \
             `nixos-rebuild switch`; run `braid seal-mountpoint` to re-seal now."
        )),
        (Some(true), ImmutabilityProbe::Immutable) => ImmutableFinding::Failure(format!(
            "{mount_point} is mounted but its inode is immutable -- a live pool root must never be \
             sealed. This should not happen with braid; investigate external interference or a bug \
             before the next lock."
        )),
        // Healthy steady states: offline+immutable (sealed) and online+mutable
        // (the live fs governs writes).
        (Some(false), ImmutabilityProbe::Immutable) | (Some(true), ImmutabilityProbe::Mutable) => {
            ImmutableFinding::None
        }
    }
}

/// Under the boot-only seal model this is the sole non-boot detection signal
/// for a mountpoint left mutable out-of-band: it warns while offline-and-mutable
/// and the next boot/activation re-seals. The branch logic lives in the pure
/// `classify_mountpoint_immutability` so it is testable without `DoctorContext`'s
/// non-injectable `online_ops`.
fn check_mountpoint_immutable<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let name = "mountpoint_immutable";
    let Some(config) = ctx.config.as_ref() else {
        return CheckResult::skip(name, "skipped (config not available)");
    };
    if !config.systemd_lifecycle() {
        return CheckResult::skip(
            name,
            "skipped (systemd_lifecycle not configured -- the mountpoint seal is module-managed)",
        );
    }
    let mount_point = config.mount_point().to_owned();
    let mp = Path::new(mount_point.as_str());

    // Tri-state mount state straight from is_mountpoint: Ok -> Some, Err ->
    // None. NOT ensure_mountpoint_is_mounted -- its unwrap_or(false) would let a
    // probe error masquerade as "offline" and fire a false offline+mutable Warn.
    let mounted = match ctx.online_ops.is_mountpoint(mp) {
        Ok(is_mounted) => Some(is_mounted),
        Err(_) => None,
    };
    let probe = ImmutabilityProbe::from_result(RealMountpointGuard.is_immutable(mp));

    match classify_mountpoint_immutability(mount_point.as_str(), mounted, probe) {
        ImmutableFinding::Warn(msg) => CheckResult::warn(name, msg),
        ImmutableFinding::Failure(msg) => CheckResult::fail(name, msg),
        // The pure classifier collapses "healthy" and "indeterminate" into None;
        // render the healthy states as Ok and the indeterminate ones as Skip so
        // an unsupported root is not falsely reported as sealed.
        ImmutableFinding::None => match (mounted, probe) {
            (Some(false), ImmutabilityProbe::Immutable) => CheckResult::ok(
                name,
                format!(
                    "{} is immutable while the pool is offline",
                    mount_point.as_str()
                ),
            ),
            (Some(true), ImmutabilityProbe::Mutable) => CheckResult::ok(
                name,
                "pool is mounted -- the live filesystem governs writes",
            ),
            (None, _) => CheckResult::skip(
                name,
                "skipped (could not determine whether the pool is mounted)",
            ),
            (_, ImmutabilityProbe::Indeterminate) => CheckResult::skip(
                name,
                "skipped (could not read the immutable attribute -- see braid-seal-mountpoint logs)",
            ),
            _ => CheckResult::ok(name, "no action needed"),
        },
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
        online_ops: RealOnlineStateOps::new(runner),
        fs,
        paths,
        mountpoint_is_mounted: None,
        df_snapshot: None,
        device_usage: None,
        pool_state: None,
    };

    let mut checks = vec![
        check_config_file(&mut ctx),
        check_config_schema(&mut ctx),
        check_config_permissions(&mut ctx),
        check_declared_disks(&mut ctx),
        check_pool_missing_devices(&mut ctx),
        check_enospc_risk(&mut ctx),
        check_foreign_luks_uuid(&mut ctx),
        check_data_profile_mismatch(&mut ctx),
        check_metadata_profile_mismatch(&mut ctx),
        check_system_profile_mismatch(&mut ctx),
        check_metadata_enospc_pressure(&mut ctx),
        check_paused_balance(&mut ctx),
    ];
    checks.extend(check_smart_selftests(&mut ctx));
    checks.push(check_beep_path(&mut ctx, options));
    checks.push(check_ups_daemon_up(&mut ctx));
    checks.push(check_braid_online_active_when_mounted(&mut ctx));
    checks.push(check_mountpoint_immutable(&mut ctx));
    checks.push(check_wake_on_lan(&mut ctx));

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
            "enospc_risk" => "enospc risk",
            "foreign_luks_uuid" => "foreign uuids",
            "data_profile_mismatch" => "data profiles",
            "metadata_profile_mismatch" => "meta profiles",
            "system_profile_mismatch" => "system profiles",
            "metadata_enospc_pressure" => "meta pressure",
            "paused_balance" => "paused balance",
            "smart_self_test" => "smart selftest",
            // The internal identifier `beep_path` stays stable for the JSON
            // schema; the human label reflects the product framing — what
            // the operator hears, not what the code does.
            "beep_path" => "alert beep",
            "ups_daemon" => "ups daemon",
            "braid_online_active" => "braid-online",
            "mountpoint_immutable" => "mountpoint seal",
            "wake_on_lan" => "wake-on-lan",
            other => other,
        };
        let display_label = match c.subject.as_deref() {
            Some(subject) => format!("{label} {subject}"),
            None => label.to_owned(),
        };
        out.push_str(&status_line(
            tag,
            color_enabled,
            &format!("{display_label:<14}  {}", c.message),
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

impl DoctorReport {
    /// Single source of truth for `braid doctor`'s exit-code contract: a `Fail`
    /// report fails the command, while `Warn`/`Ok`/`Skip` reports succeed.
    pub(crate) fn command_result(&self) -> Result<(), DoctorError> {
        match self.status {
            CheckStatus::Fail => Err(DoctorError::Failed),
            _ => Ok(()),
        }
    }
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

    report.command_result()
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
            online_ops: RealOnlineStateOps::new(runner),
            fs,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
            device_usage: None,
            pool_state: None,
        }
    }

    pub(crate) fn for_test_beep(runner: &'a R, paths: &'a StatePaths) -> Self {
        Self {
            config_path: PathBuf::new(),
            config_value: None,
            config: None,
            runner,
            online_ops: RealOnlineStateOps::new(runner),
            fs: &REAL_FILESYSTEM_FOR_TESTS,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
            device_usage: None,
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
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;
    use crate::test_fixtures::{
        DF_METADATA_20_USED, DF_METADATA_78_USED, DF_MIXED, DF_MIXED_METADATA, DF_RAID1_CLEAN,
        DeviceUsageSpec, DfQueryFailureRunner, DoctorMockFs, PoolMissingDevicesRunner,
        UpscSpawnFailureRunner, beep_ctx, cls, config_with_ups_enabled, config_without_ups,
        device_usage_raw, device_usage_raw_body, device_usage_three_one_tight,
        device_usage_three_two_tight, device_usage_two_healthy, device_usage_two_tight, df_json,
        df_json_fail, disk_member_with, human_options, is_luks_ok, isolated_paths, luks_uuid_ok,
        mountpoint_fail, mountpoint_ok, parsed_doctor_ctx, pool_state_runner,
        smart_selftest_runner_for, smartctl_selftest_json, systemctl_show_active_state_output,
        test_uuid, unlock_btrfs_balance_status_idle, unlock_btrfs_balance_status_paused,
        unlock_btrfs_balance_status_paused_skip_balance, ups_ctx, valid_config_json, write_temp,
    };
    use crate::types::MountPoint;

    fn find_check<'a>(report: &'a DoctorReport, name: &str) -> &'a CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn enospc_device_usage(unallocated: &[u64], device_size: u64) -> String {
        let specs: Vec<_> = unallocated
            .iter()
            .enumerate()
            .map(|(index, unallocated)| {
                let devid = (index + 1) as u64;
                DeviceUsageSpec::live(
                    &format!("/dev/mapper/braid-disk{devid}"),
                    devid,
                    device_size,
                    &[],
                    *unallocated,
                )
            })
            .collect();
        device_usage_raw_body(&specs)
    }

    fn enospc_three_disk_runner(usage: &str) -> MockRunner {
        let (usage_req, usage_out) = device_usage_raw(usage);
        pool_state_runner(
            vec![
                ("braid-disk1", 1, "/dev/vdb", test_uuid(1)),
                ("braid-disk2", 2, "/dev/vdc", test_uuid(2)),
                ("braid-disk3", 3, "/dev/vdd", test_uuid(3)),
            ],
            &[],
        )
        .with_output(usage_req, usage_out)
    }

    fn doctor_smartctl_selftest_json(
        stdout: impl Into<String>,
        exit_status: i32,
    ) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "smartctl --json -A -l selftest /dev/disk/by-id/disk1".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status,
        }
    }

    fn selftest_results_for(runner: &impl CommandRunner, paths: &StatePaths) -> Vec<CheckResult> {
        let mut ctx = parsed_doctor_ctx(runner, paths);
        check_smart_selftests(&mut ctx)
    }

    fn single_selftest_results(
        stdout: impl Into<String>,
        exit_status: i32,
    ) -> (tempfile::TempDir, StatePaths, MockRunner, Vec<CheckResult>) {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(1))]);
        let runner = MockRunner::default().with_output(
            CmdRequest::SmartctlSelftestLogJson {
                device: "/dev/disk/by-id/disk1".to_owned(),
            },
            doctor_smartctl_selftest_json(stdout, exit_status),
        );
        let results = selftest_results_for(&runner, &paths);
        (dir, paths, runner, results)
    }

    fn single_selftest_fixture_results(
        fixture: &str,
        exit_status: i32,
    ) -> (tempfile::TempDir, StatePaths, MockRunner, Vec<CheckResult>) {
        let (_, output) = smartctl_selftest_json("/dev/disk/by-id/disk1", fixture, exit_status);
        single_selftest_results(output.stdout, output.exit_status)
    }

    fn by_subject<'a>(results: &'a [CheckResult], subject: &str) -> &'a CheckResult {
        results
            .iter()
            .find(|r| r.subject.as_deref() == Some(subject))
            .unwrap_or_else(|| panic!("no result for subject {subject}"))
    }

    fn only_result(results: &[CheckResult]) -> &CheckResult {
        assert_eq!(results.len(), 1, "expected one result: {results:?}");
        &results[0]
    }

    fn save_doctor_membership(paths: &StatePaths, entries: &[(u64, &str, &str, Option<u64>)]) {
        let mut m = membership::PoolMembership::empty();
        for (seed, name, by_id, devid) in entries {
            let (uuid, member) = disk_member_with(*seed, name, by_id, *devid, None);
            m.insert(uuid, member).expect("fixture member inserts");
        }
        membership::save_membership(&m, paths).expect("fixture membership saves");
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
        let mut actual_names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        actual_names.sort();
        let expected_names: Vec<&str> = vec![
            "beep_path",
            "braid_online_active",
            "config_file",
            "config_permissions",
            "config_schema",
            "data_profile_mismatch",
            "declared_disks",
            "enospc_risk",
            "foreign_luks_uuid",
            "metadata_enospc_pressure",
            "metadata_profile_mismatch",
            "mountpoint_immutable",
            "paused_balance",
            "pool_missing_devices",
            "smart_self_test",
            "system_profile_mismatch",
            "ups_daemon",
            "wake_on_lan",
        ];
        assert_eq!(actual_names, expected_names);
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Ok);
        let selftest = find_check(&report, "smart_self_test");
        assert_eq!(selftest.status, CheckStatus::Skip);
        assert_eq!(selftest.subject, None);
        // declared_disks skips since no pool membership file exists in test env
        assert_eq!(
            find_check(&report, "declared_disks").status,
            CheckStatus::Skip
        );
        // beep_path is intentionally not asserted here: it depends on real host
        // state (/etc/braid/notifier-config.json). Deterministic coverage
        // lives in the check_beep_path_inner tests.
    }

    // Intent: a recent completed self-test produces one per-drive Ok row.
    // Why it exists: doctor is the user-facing surface for stale self-test
    //   logs, so the fresh path must be quiet and scoped to the member name.
    // Scenario: disk1 completed a short test roughly two powered-on days ago.
    #[test]
    fn check_smart_selftest_recent_pass() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-recent-pass.json", 0);
        let r = only_result(&results);
        assert_eq!(r.name, "smart_self_test");
        assert_eq!(r.subject.as_deref(), Some("disk1"));
        assert_eq!(r.status, CheckStatus::Ok);
        assert!(r.message.contains("passed ~2 days ago"), "{}", r.message);
    }

    // Intent: bit-7 smartctl exits remain a Fail, not a command-error Skip.
    // Why it exists: bit 7 is smartctl's active self-test error signal.
    // Scenario: smartctl exits 128 with a failed extended-test JSON body.
    #[test]
    fn check_smart_selftest_active_failure_exit_128() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-active-failure.json", 128);
        let r = only_result(&results);
        assert_eq!(r.subject.as_deref(), Some("disk1"));
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("FAILED"), "{}", r.message);
    }

    // Intent: superseded failures do not fail doctor.
    // Why it exists: smartctl already reports active vs outdated failures, and
    //   braid should consume that contract directly.
    // Scenario: an old failed short test is followed by a passing extended test.
    #[test]
    fn check_smart_selftest_outdated_failure_not_fail() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-failure-outdated.json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Ok);
        assert!(!r.message.contains("FAILED"), "{}", r.message);
    }

    // Intent: a newer passing short test does not clear a failed extended test.
    // Why it exists: failure supersession is smartctl-specific and must not be
    //   re-derived from "latest passing entry" alone.
    // Scenario: disk1 has a passing short entry newer than an active failed
    //   extended entry.
    #[test]
    fn check_smart_selftest_passing_short_does_not_clear_extended_failure() {
        let (_dir, _paths, _runner, results) = single_selftest_fixture_results(
            "smartctl-selftest-ata-short-pass-does-not-supersede.json",
            128,
        );
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("FAILED"), "{}", r.message);
    }

    // Intent: smartctl spawn failures degrade to a scoped Skip row.
    // Why it exists: missing smartctl should not panic or hide the drive row.
    // Scenario: the runner cannot spawn smartctl for disk1.
    #[test]
    fn check_smart_selftest_runner_spawn_failure_skips() {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(1))]);
        let runner = MockRunner::default().with_handler(|request| match request {
            CmdRequest::SmartctlSelftestLogJson { .. } => {
                Some(Err(CmdError::Failed("smartctl: not found".into())))
            }
            _ => None,
        });
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        let r = only_result(&results);
        assert_eq!(r.name, "smart_self_test");
        assert_eq!(r.subject.as_deref(), Some("disk1"));
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(r.message.contains("smartctl: not found"), "{}", r.message);
    }

    // Intent: bits 0-2 produce command-error Skip rows.
    // Why it exists: device-open or SMART-command failures make JSON unsafe to
    //   trust for self-test classification.
    // Scenario: smartctl exits with bit 1 and no stdout.
    #[test]
    fn check_smart_selftest_smartctl_errors_bit_0_2_empty_stdout() {
        let (_dir, _paths, _runner, results) = single_selftest_results("", 2);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(
            r.message,
            "SMART self-test status unavailable (smartctl command failed)"
        );
    }

    // Intent: command-error bits win over parseable stdout.
    // Why it exists: a partial JSON body must not bypass the bits 0-2 guard.
    // Scenario: smartctl exits with bit 2 and still prints valid ATA JSON.
    #[test]
    fn check_smart_selftest_command_error_with_nonempty_stdout() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-command-error.json", 4);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(
            r.message,
            "SMART self-test status unavailable (smartctl command failed)"
        );
    }

    // Intent: bad smartctl JSON becomes a parse-failure Skip.
    // Why it exists: operator output should distinguish corrupt output from
    //   unsupported protocols or active drive failures.
    // Scenario: smartctl exits 0 but stdout is not JSON.
    #[test]
    fn check_smart_selftest_parse_failure_skips() {
        let (_dir, _paths, _runner, results) = single_selftest_results("not json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(
            r.message,
            "SMART self-test status unavailable (smartctl JSON output not parseable)"
        );
    }

    // Intent: missing power-on hours skips age-based classification.
    // Why it exists: without attribute 9, doctor cannot determine staleness.
    // Scenario: ATA self-test rows are present but `power_on_time` is absent.
    #[test]
    fn check_smart_selftest_missing_power_on_time_skips() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 0,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 0, "string": "Completed without error"},
                            "lifetime_hours": 4990
                        }
                    ]
                }
            }
        }"#;
        let (_dir, _paths, _runner, results) = single_selftest_results(json, 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(
            r.message.contains("power_on_time.hours missing"),
            "{}",
            r.message
        );
    }

    // Intent: active failures are reported before the missing-POH gate.
    // Why it exists: failure counters do not depend on power-on-hour age math.
    // Scenario: an active failed extended test is present, but attribute 9 is
    //   missing from smartctl JSON.
    #[test]
    fn check_smart_selftest_active_failure_without_poh_still_fails() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 1,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 2, "string": "Extended"},
                            "status": {"value": 80, "string": "Completed: electrical failure"},
                            "lifetime_hours": 4900
                        }
                    ]
                }
            }
        }"#;
        let (_dir, _paths, _runner, results) = single_selftest_results(json, 128);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("FAILED"), "{}", r.message);
    }

    // Intent: fatal/unknown ATA status codes produce Fail rows.
    // Why it exists: smartctl omits `status.passed` for status 0x3, so
    //   classification must use `status.value`.
    // Scenario: disk1 has one fatal-or-unknown extended self-test entry.
    #[test]
    fn check_smart_selftest_fatal_or_unknown() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-fatal-or-unknown.json", 128);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.message.contains("FAILED"), "{}", r.message);
    }

    // Intent: aborted-only logs warn with the "never" message.
    // Why it exists: non-empty logs still may contain no completed passing
    //   self-test.
    // Scenario: disk1 has only aborted/interrupted self-test rows.
    #[test]
    fn check_smart_selftest_aborted_only_warns_never() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-aborted-only.json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.message.contains("no completed SMART self-test recorded"),
            "{}",
            r.message
        );
        assert!(!r.message.contains('~'), "{}", r.message);
    }

    // Intent: truly empty logs warn with the "never" message.
    // Why it exists: smartctl's empty-log shape omits the table entirely.
    // Scenario: a drive has never recorded a self-test.
    #[test]
    fn check_smart_selftest_empty_log_warns_never() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-empty.json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("no completed SMART self-test recorded"));
    }

    // Intent: stale passing self-tests warn with an age and paste-ready hint.
    // Why it exists: the new check's primary purpose is to surface stale logs
    //   without scheduling tests itself.
    // Scenario: disk1's newest passing test is 3000 powered-on hours old.
    #[test]
    fn check_smart_selftest_stale_warns_with_age() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-stale.json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("~125 days"), "{}", r.message);
        assert!(
            r.message.contains("run: smartctl -t short"),
            "{}",
            r.message
        );
    }

    // Intent: the singular-day boundary renders without a trailing `s`.
    // Why it exists: doctor output has pinned singular/plural wording for
    //   operator-facing counts.
    // Scenario: the last passing entry is exactly 24 powered-on hours old.
    #[test]
    fn check_smart_selftest_ok_uses_singular_day_at_boundary() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "power_on_time": {"hours": 5000},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 0,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 0, "string": "Completed without error"},
                            "lifetime_hours": 4976
                        }
                    ]
                }
            }
        }"#;
        let (_dir, _paths, _runner, results) = single_selftest_results(json, 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Ok);
        assert!(r.message.contains("passed ~1 day ago"), "{}", r.message);
    }

    // Intent: age phrases truncate powered-on hours to whole days.
    // Why it exists: both Ok and stale-Warn branches share this formatter.
    // Scenario: boundary values around 0, 1, 2, and 90 days.
    #[test]
    fn approx_days_phrase_pluralisation() {
        assert_eq!(approx_days_phrase(0), "~0 days");
        assert_eq!(approx_days_phrase(23), "~0 days");
        assert_eq!(approx_days_phrase(24), "~1 day");
        assert_eq!(approx_days_phrase(47), "~1 day");
        assert_eq!(approx_days_phrase(48), "~2 days");
        assert_eq!(
            approx_days_phrase(STALE_SELFTEST_THRESHOLD_HOURS),
            "~90 days"
        );
    }

    // Intent: NVMe drives Skip with the protocol named.
    // Why it exists: NVMe self-test logs are out of v1 scope and have a
    //   different schema from ATA.
    // Scenario: smartctl reports `device.protocol = "NVMe"`.
    #[test]
    fn check_smart_selftest_nvme_skips_with_protocol_reason() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-nvme-unsupported.json", 0);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Skip);
        assert!(r.message.contains("NVMe"), "{}", r.message);
    }

    // Intent: non-ATA or missing protocols Skip with deterministic reasons.
    // Why it exists: self-test classification must not default missing
    //   protocol to SATA the way health parsing does.
    // Scenario: smartctl reports SCSI or omits protocol entirely.
    #[test]
    fn check_smart_selftest_scsi_or_missing_protocol_skips() {
        for (json, expected) in [
            (r#"{"device":{"protocol":"SCSI"}}"#, "SCSI"),
            (r#"{"device":{}}"#, "unknown"),
        ] {
            let (_dir, _paths, _runner, results) = single_selftest_results(json, 0);
            let r = only_result(&results);
            assert_eq!(r.status, CheckStatus::Skip);
            assert!(r.message.contains(expected), "{}", r.message);
        }
    }

    // Intent: active-error counters without a parsed failure use the fallback
    //   Fail message.
    // Why it exists: parser drift should still surface a high-severity result
    //   if smartctl's counters report active failures.
    // Scenario: counters report one active error but the table contains only
    //   an aborted entry.
    #[test]
    fn check_smart_selftest_active_errors_fallback_message() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "power_on_time": {"hours": 5000},
            "ata_smart_self_test_log": {
                "standard": {
                    "count": 1,
                    "error_count_total": 1,
                    "error_count_outdated": 0,
                    "table": [
                        {
                            "type": {"value": 1, "string": "Short"},
                            "status": {"value": 16, "string": "Aborted by host"},
                            "lifetime_hours": 4990
                        }
                    ]
                }
            }
        }"#;
        let (_dir, _paths, _runner, results) = single_selftest_results(json, 128);
        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.message
                .contains("reports 1 active failure(s) but no failure entry was parsed"),
            "{}",
            r.message
        );
    }

    // Intent: the check emits one stable-name row per member.
    // Why it exists: dynamic names like `smart_self_test_disk1` would break
    //   machine consumers.
    // Scenario: a three-drive pool has recent passing self-test fixtures for
    //   every member.
    #[test]
    fn check_smart_selftest_emits_one_result_per_drive() {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[
                (1, "disk1", "/dev/disk/by-id/disk1", Some(1)),
                (2, "disk2", "/dev/disk/by-id/disk2", Some(2)),
                (3, "disk3", "/dev/disk/by-id/disk3", Some(3)),
            ],
        );
        let runner = smart_selftest_runner_for(&[
            (
                "/dev/disk/by-id/disk1",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
            (
                "/dev/disk/by-id/disk2",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
            (
                "/dev/disk/by-id/disk3",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
        ]);
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.name == "smart_self_test"));
        let subjects: Vec<&str> = results
            .iter()
            .map(|r| r.subject.as_deref().expect("per-drive subject"))
            .collect();
        assert_eq!(subjects, vec!["disk1", "disk2", "disk3"]);
    }

    // Intent: per-drive SMART rows preserve membership display order.
    // Why it exists: doctor output should be stable across runs regardless of
    //   UUID ordering in pool.json.
    // Scenario: pool membership is inserted out of display order.
    #[test]
    fn check_smart_selftest_preserves_membership_order() {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[
                (3, "zeta", "/dev/disk/by-id/zeta", Some(3)),
                (1, "alpha", "/dev/disk/by-id/alpha", Some(1)),
                (2, "middle", "/dev/disk/by-id/middle", Some(2)),
            ],
        );
        let runner = smart_selftest_runner_for(&[
            (
                "/dev/disk/by-id/zeta",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
            (
                "/dev/disk/by-id/alpha",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
            (
                "/dev/disk/by-id/middle",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
        ]);
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        let subjects: Vec<&str> = results
            .iter()
            .map(|r| r.subject.as_deref().unwrap())
            .collect();
        assert_eq!(subjects, vec!["alpha", "middle", "zeta"]);
    }

    // Intent: mixed per-drive statuses stay isolated to their own rows.
    // Why it exists: one stale or failed drive must not pollute another
    //   drive's message body.
    // Scenario: disk1 is fresh, disk2 is stale, and disk3 has an active
    //   self-test failure.
    #[test]
    fn check_smart_selftest_mixed_statuses_one_per_drive() {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[
                (1, "disk1", "/dev/disk/by-id/disk1", Some(1)),
                (2, "disk2", "/dev/disk/by-id/disk2", Some(2)),
                (3, "disk3", "/dev/disk/by-id/disk3", Some(3)),
            ],
        );
        let runner = smart_selftest_runner_for(&[
            (
                "/dev/disk/by-id/disk1",
                "smartctl-selftest-ata-recent-pass.json",
                0,
            ),
            (
                "/dev/disk/by-id/disk2",
                "smartctl-selftest-ata-stale.json",
                0,
            ),
            (
                "/dev/disk/by-id/disk3",
                "smartctl-selftest-ata-active-failure.json",
                128,
            ),
        ]);
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        assert_eq!(by_subject(&results, "disk1").status, CheckStatus::Ok);
        assert_eq!(by_subject(&results, "disk2").status, CheckStatus::Warn);
        assert_eq!(by_subject(&results, "disk3").status, CheckStatus::Fail);
        assert!(!by_subject(&results, "disk1").message.contains("FAILED"));
    }

    // Intent: membership load errors emit the only unscoped self-test row.
    // Why it exists: without membership there is no stable disk subject to
    //   attach to the result.
    // Scenario: pool.json is absent.
    #[test]
    fn check_smart_selftest_membership_load_error_emits_unscoped_skip() {
        let (dir, paths) = isolated_paths();
        let runner = MockRunner::default();
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        let r = only_result(&results);
        assert_eq!(r.name, "smart_self_test");
        assert_eq!(r.subject, None);
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(r.message, "skipped (no pool membership file)");
    }

    // Intent: corrupt membership emits the only unscoped self-test row as Warn.
    // Why it exists: corrupt pool.json is an operator problem, not an absent
    //   pool-membership setup state.
    // Scenario: pool.json exists but does not satisfy the PoolMembership schema.
    #[test]
    fn smart_selftest_warns_on_corrupt_membership() {
        let (dir, paths) = isolated_paths();
        std::fs::write(paths.pool_json(), "{}").expect("corrupt membership writes");
        let runner = MockRunner::default();
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        let r = only_result(&results);
        assert_eq!(r.name, "smart_self_test");
        assert_eq!(r.subject, None);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.message.contains("could not load pool membership"),
            "{}",
            r.message
        );
    }

    // Intent: empty membership emits one unscoped Skip row.
    // Why it exists: an empty pool.json is enumerable but has no per-drive
    //   subjects.
    // Scenario: pool membership parses with zero disks.
    #[test]
    fn check_smart_selftest_no_members_emits_unscoped_skip() {
        let (dir, paths) = isolated_paths();
        membership::save_membership(&membership::PoolMembership::empty(), &paths)
            .expect("empty membership saves");
        let runner = MockRunner::default();
        let results = selftest_results_for(&runner, &paths);
        drop(dir);
        let r = only_result(&results);
        assert_eq!(r.name, "smart_self_test");
        assert_eq!(r.subject, None);
        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(r.message, "skipped (no pool members declared)");
    }

    // Intent: warning hints include the literal by-id path.
    // Why it exists: the operator should be able to paste the suggested
    //   smartctl command without replacing placeholders.
    // Scenario: disk1 has no completed self-test.
    #[test]
    fn check_smart_selftest_message_contains_by_id_path() {
        let (_dir, _paths, _runner, results) =
            single_selftest_fixture_results("smartctl-selftest-ata-empty.json", 0);
        let r = only_result(&results);
        assert!(r.message.contains("/dev/disk/by-id/disk1"), "{}", r.message);
    }

    // Intent: self-test status for a present member is queried through the
    //   live backing path.
    // Why it exists: a drifted by-id path must not make doctor report stale
    //   or missing SMART self-test data for a currently mounted member.
    // Scenario: disk1 is present at /dev/vdb; /dev/vdb has a recent passing
    //   self-test, while the persisted by-id mock has a stale result.
    #[test]
    fn check_smart_selftest_present_member_queries_live_underlying() {
        let (dir, paths) = isolated_paths();
        save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(1))]);
        let (live_req, live_out) =
            smartctl_selftest_json("/dev/vdb", "smartctl-selftest-ata-recent-pass.json", 0);
        let (by_id_req, by_id_out) = smartctl_selftest_json(
            "/dev/disk/by-id/disk1",
            "smartctl-selftest-ata-stale.json",
            0,
        );
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[])
            .with_output(live_req, live_out)
            .with_output(by_id_req, by_id_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let results = check_smart_selftests(&mut ctx);
        drop(dir);

        let r = only_result(&results);
        assert_eq!(r.status, CheckStatus::Ok);
        assert!(r.message.contains("passed ~2 days ago"), "{}", r.message);
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

    // Intent: extra top-level config fields fail schema validation.
    // Why it exists: stale or misspelled runtime config keys must surface
    // instead of falling back to defaults silently.
    // Scenario: operator keeps an old config.json with a removed `disks` key.
    #[test]
    fn valid_json_with_extra_fields_fails_schema() {
        let f = write_temp(r#"{"disks":{},"mount_point":"/mnt/storage"}"#);
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );
        assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
        assert_eq!(
            find_check(&report, "config_schema").status,
            CheckStatus::Fail
        );
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
                subject: None,
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Warn,
                message: "".into(),
                subject: None,
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Warn);

        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Warn,
                message: "".into(),
                subject: None,
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "".into(),
                subject: None,
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Fail);
    }

    // Intent: `braid doctor` fails the command only for Fail reports.
    // Why it exists: the process exit contract belongs to report status, not
    //   any individual doctor check or VM scenario.
    // Scenario: a future change accidentally makes Warn fail, or lets Fail
    //   succeed, when translating a report into a command result.
    #[test]
    fn doctor_report_command_result_fails_only_on_fail() {
        for (status, should_fail) in [
            (CheckStatus::Ok, false),
            (CheckStatus::Warn, false),
            (CheckStatus::Fail, true),
            (CheckStatus::Skip, false),
        ] {
            let report = DoctorReport {
                status,
                checks: vec![],
            };
            assert_eq!(report.command_result().is_err(), should_fail, "{status:?}");
        }
    }

    // Intent: any Fail-producing check escalates to command failure.
    // Why it exists: per-check failure scenarios should rely on the shared
    //   worst-status and status-to-exit contract instead of duplicating exit
    //   assertions through every checker.
    // Scenario: one passing check and one failing check produce an overall Fail
    //   report, and `braid doctor` exits non-zero from that report.
    #[test]
    fn any_fail_check_escalates_to_command_failure() {
        let checks = vec![CheckResult::ok("a", ""), CheckResult::fail("b", "boom")];
        let status = overall_status(&checks);
        let report = DoctorReport { status, checks };

        assert_eq!(report.status, CheckStatus::Fail);
        assert!(report.command_result().is_err());
    }

    #[test]
    fn skip_does_not_affect_overall() {
        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Ok,
                message: "".into(),
                subject: None,
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Skip,
                message: "".into(),
                subject: None,
            },
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Ok);

        let checks = vec![
            CheckResult {
                name: "a".into(),
                status: CheckStatus::Skip,
                message: "".into(),
                subject: None,
            },
            CheckResult {
                name: "b".into(),
                status: CheckStatus::Skip,
                message: "".into(),
                subject: None,
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
                subject: None,
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""status":"ok""#), "overall: {json}");
        assert!(json.contains(r#""status":"fail""#), "check: {json}");
        assert!(!json.contains("Ok"));
        assert!(!json.contains("Fail"));
    }

    // Intent: `subject: None` is omitted from JSON.
    // Why it exists: adding per-drive subjects must not add `"subject": null`
    //   noise to every existing doctor check.
    // Scenario: an existing unscoped check serializes after the schema change.
    #[test]
    fn json_serialization_subject_none_omits_field() {
        let check = CheckResult {
            name: "config_file".into(),
            status: CheckStatus::Ok,
            message: "ok".into(),
            subject: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(!json.contains("subject"), "{json}");
    }

    // Intent: `subject: Some` is visible in JSON.
    // Why it exists: per-drive SMART self-test rows need a stable disk
    //   identity without dynamic check names.
    // Scenario: a smart self-test row is serialized for disk1.
    #[test]
    fn json_serialization_subject_some_emits_field() {
        let check = CheckResult {
            name: "smart_self_test".into(),
            status: CheckStatus::Ok,
            message: "passed ~2 days ago".into(),
            subject: Some("disk1".into()),
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains(r#""subject":"disk1""#), "{json}");
    }

    // Intent: missing and present subjects both round-trip through JSON.
    // Why it exists: `#[serde(default)]` is required so old JSON rows without
    //   `subject` still deserialize.
    // Scenario: one unscoped check and one per-drive check are round-tripped.
    #[test]
    fn json_roundtrip_preserves_subject() {
        for check in [
            CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Ok,
                message: "ok".into(),
                subject: None,
            },
            CheckResult {
                name: "smart_self_test".into(),
                status: CheckStatus::Warn,
                message: "stale".into(),
                subject: Some("disk1".into()),
            },
        ] {
            let json = serde_json::to_string(&check).unwrap();
            let decoded: CheckResult = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, check);
        }
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
                    subject: None,
                },
                CheckResult {
                    name: "config_permissions".into(),
                    status: CheckStatus::Warn,
                    message: "world-writable".into(),
                    subject: None,
                },
                CheckResult {
                    name: "declared_disks".into(),
                    status: CheckStatus::Fail,
                    message: "missing disk1".into(),
                    subject: None,
                },
                CheckResult {
                    name: "pool_missing_devices".into(),
                    status: CheckStatus::Skip,
                    message: "pool offline".into(),
                    subject: None,
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

    // Intent: subject-bearing rows render label and subject together.
    // Why it exists: human output should show the disk next to the stable
    //   `smart selftest` label while JSON keeps the disk in `subject`.
    // Scenario: one Ok SMART self-test row for disk1 is rendered.
    #[test]
    fn format_subject_rendered_after_label() {
        let report = DoctorReport {
            status: CheckStatus::Ok,
            checks: vec![CheckResult {
                name: "smart_self_test".into(),
                status: CheckStatus::Ok,
                message: "passed ~2 days ago".into(),
                subject: Some("disk1".into()),
            }],
        };
        let human = format_doctor_human_with(&report, false);
        assert!(
            human.contains("smart selftest disk1  passed ~2 days ago"),
            "{human}"
        );
    }

    // Intent: unscoped rows keep the existing human formatter shape.
    // Why it exists: adding subject support must not reflow established doctor
    //   lines that do not set `subject`.
    // Scenario: a config_file row renders byte-identically to the legacy shape.
    #[test]
    fn format_subject_none_renders_existing_shape() {
        let report = DoctorReport {
            status: CheckStatus::Ok,
            checks: vec![CheckResult {
                name: "config_file".into(),
                status: CheckStatus::Ok,
                message: "present".into(),
                subject: None,
            }],
        };
        let human = format_doctor_human_with(&report, false);
        assert_eq!(human, "[ok]   config file     present\n");
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

    // Intent: declared_disks treats an empty pool.json as no declared members.
    // Why it exists: an empty membership must not render as "all 0 declared
    //   disks present".
    // Scenario: pool.json parses successfully but has no disk entries.
    #[test]
    fn declared_disks_skips_when_membership_is_empty() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        membership::save_membership(&membership::PoolMembership::empty(), &paths)
            .expect("empty membership saves");
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (no pool members declared)");
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

    // Intent: declared_disks warns when pool.json exists but is corrupt.
    // Why it exists: corrupt authoritative membership is diagnosable operator
    //   state, not a first-run missing-membership condition.
    // Scenario: an operator or interrupted write leaves pool.json as valid JSON
    //   that does not match the PoolMembership schema.
    #[test]
    fn declared_disks_warns_on_corrupt_membership() {
        let f = write_temp(valid_config_json());
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.pool_json(), "{}").expect("corrupt membership writes");
        let report = run_doctor(
            f.path(),
            &MockRunner::default(),
            &RealFilesystem,
            &paths,
            human_options(),
        );
        let check = find_check(&report, "declared_disks");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not load pool membership"),
            "{}",
            check.message
        );
    }

    // Intent: declared_disks reports a mounted-pool topology probe error
    //   alongside normal per-member classification.
    // Why it exists: a mounted pool whose live topology cannot be probed must
    //   not be treated like an offline pool and silently reported healthy.
    // Scenario: btrfs topology probing fails while pool.json still names a
    //   declared member whose by-id path is currently missing.
    #[test]
    fn check_declared_disks_warns_when_live_topology_unavailable() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[(1, "disk1", "/dev/disk/by-id/does-not-exist", None)],
        );
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        ctx.mountpoint_is_mounted = Some(true);
        ctx.pool_state = Some(Err(ProbeError::NotBtrfs {
            mount_point: "/mnt/storage".into(),
            fstype: "ext4".into(),
        }));

        let check = check_declared_disks(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("could not compare declared disks to live pool"),
            "missing topology warning: {}",
            check.message
        );
        assert!(
            check.message.contains("disk1"),
            "missing per-disk classification: {}",
            check.message
        );
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
        let result = summarize_declared_disks(&inputs, None);
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
         * Why it exists: this is the worst recoverable state -- cryptsetup's
         *   crypt_load cannot read or validate the header (it may be zeroed, or
         *   its metadata may have failed validation). Without specific
         *   guidance, users see a generic exit code from later cryptsetup
         *   operations and have no actionable next step. The negative
         *   assertions also pin the
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
        let result = summarize_declared_disks(&inputs, None);
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
        let result = summarize_declared_disks(&inputs, None);
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

    // Intent: declared_disks warns when a verified member is absent from the
    //   mounted live pool.
    // Why it exists: present, identity-verified but unassembled members must
    //   not be reported as healthy.
    // Scenario: a degraded mount assembled without one declared LUKS member.
    #[test]
    fn summarize_warn_offline_member_not_in_live_pool() {
        let inputs = [cls("disk1", "/dev/disk/by-id/wwn-0x1", DiskState::Offline)];

        let result = summarize_declared_disks(&inputs, None);

        assert_eq!(result.status, CheckStatus::Warn);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("not in the live pool"),
            "missing offline wording: {msg}"
        );
        assert!(
            !msg.contains("Action"),
            "offline must be remedy-free: {msg}"
        );
        assert!(
            !msg.contains("luksHeaderRestore"),
            "offline must not suggest header restore: {msg}"
        );
    }

    // Intent: offline members do not downgrade a LUKS UUID mismatch failure.
    // Why it exists: Fail remains reserved for the unsafe identity
    //   contradiction and must dominate cause-neutral offline rows.
    // Scenario: one declared disk is swapped while another verified member is
    //   absent from the mounted live pool.
    #[test]
    fn summarize_offline_does_not_override_uuid_mismatch_fail() {
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
            cls("disk2", "/dev/disk/by-id/wwn-0x2", DiskState::Offline),
        ];

        let result = summarize_declared_disks(&inputs, None);

        assert_eq!(result.status, CheckStatus::Fail);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing mismatch disk: {msg}");
        assert!(msg.contains("disk2"), "missing offline disk: {msg}");
        assert!(
            msg.contains("detach the foreign disk"),
            "missing mismatch guidance: {msg}"
        );
    }

    // Intent: declared_disks warns when the mounted pool's live topology
    //   cannot be probed, even with no per-disk problems.
    // Why it exists: probe failure is indeterminate mounted-pool state, not
    //   an offline pool where identity-only checks are enough.
    // Scenario: every declared member's LUKS identity verifies, but btrfs
    //   topology probing fails while the mountpoint is active.
    #[test]
    fn summarize_warn_topology_unavailable_when_probe_failed() {
        let inputs = [cls(
            "disk1",
            "/dev/disk/by-id/wwn-0x1",
            DiskState::LuksHeaderOk,
        )];

        let result = summarize_declared_disks(&inputs, Some("boom"));

        assert_eq!(result.status, CheckStatus::Warn);
        assert!(
            result
                .message
                .contains("could not compare declared disks to live pool"),
            "missing topology warning: {}",
            result.message
        );
        assert!(
            result.message.contains("boom"),
            "missing probe error: {}",
            result.message
        );
    }

    // Intent: topology probe errors do not downgrade a LUKS UUID mismatch
    //   failure.
    // Why it exists: a mounted-pool probe error is a warning-level global
    //   note, while UUID mismatch is still the fail-closed identity problem.
    // Scenario: doctor cannot probe live topology and also observes a swapped
    //   declared disk.
    #[test]
    fn summarize_topology_unavailable_does_not_override_uuid_mismatch_fail() {
        let expected = test_uuid(1);
        let observed = test_uuid(2);
        let inputs = [cls(
            "disk1",
            "/dev/disk/by-id/wwn-0x1",
            DiskState::LuksUuidMismatch {
                expected: expected.clone(),
                observed: observed.clone(),
            },
        )];

        let result = summarize_declared_disks(&inputs, Some("boom"));

        assert_eq!(result.status, CheckStatus::Fail);
        let msg = &result.message;
        assert!(
            msg.contains("detach the foreign disk"),
            "missing mismatch guidance: {msg}"
        );
        assert!(
            msg.contains("could not compare declared disks to live pool"),
            "missing topology warning: {msg}"
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
        let (uuid_req, uuid_out) = luks_uuid_ok(device, observed.as_str());
        let runner = MockRunner::default()
            .with_output(is_luks_req, is_luks_out)
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
        let (uuid_req, uuid_out) = luks_uuid_ok(device, expected.as_str());
        let runner = MockRunner::default()
            .with_output(is_luks_req, is_luks_out)
            .with_output(uuid_req, uuid_out);

        let state = classify_luks_identity(&runner, device, &expected);

        match state {
            DiskState::LuksHeaderOk => {}
            other => panic!("expected LuksHeaderOk, got {other:?}"),
        }
    }

    // Intent: classify_luks_identity probes a declared disk with exactly
    //   `cryptsetup isLuks` then `cryptsetup luksUUID`, and never `luksDump`.
    // Why it exists: doctor is a read-only diagnostic, but isLuks/crypt_load can
    //   auto-recover (write) a one-good-copy LUKS2 header under metadata locking;
    //   a redundant second crypt_load probe (luksDump) would multiply that write
    //   surface for no gain. The DiskState doc already drifted once (commit
    //   3ff2ec15) claiming luksDump was part of the probe -- pin the wiring so a
    //   re-added dump call fails loudly instead of passing silently against a
    //   leftover optional mock.
    // Scenario: a healthy declared member at its by-id path whose live UUID matches
    //   pool.json; the probe must touch the device exactly twice, in order.
    #[test]
    fn classify_luks_identity_issues_isluks_then_luksuuid_only() {
        let device = "/dev/disk/by-id/wwn-0x1";
        let expected = test_uuid(1);
        let (is_luks_req, is_luks_out) = is_luks_ok(device);
        let (uuid_req, uuid_out) = luks_uuid_ok(device, expected.as_str());
        let runner = MockRunner::default()
            .with_output(is_luks_req, is_luks_out)
            .with_output(uuid_req, uuid_out);

        let state = classify_luks_identity(&runner, device, &expected);

        // Sanity: the path actually completed (not short-circuited to ProbeFailed).
        assert!(matches!(state, DiskState::LuksHeaderOk));
        // Load-bearing: exact request set pins presence (isLuks + luksUUID),
        // order, count, and absence of any dump variant or other probe.
        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupIsLuks {
                    device: device.to_owned(),
                },
                CmdRequest::CryptsetupLuksUuid {
                    device: device.to_owned(),
                },
            ],
        );
    }

    // Intent: live-pool reconciliation marks verified members absent from the
    //   assembled UUID set as offline.
    // Why it exists: doctor must surface the status-side blind spot where a
    //   valid declared disk is present but not part of the mounted pool.
    // Scenario: a degraded mount includes one declared member and omits
    //   another whose raw LUKS header still verifies.
    #[test]
    fn reconcile_marks_verified_member_offline_when_absent_from_live_pool() {
        let uuid = test_uuid(1);
        let topology = LiveTopology::Online(HashSet::from([test_uuid(2)]));

        let state = reconcile_with_live_pool(&uuid, DiskState::LuksHeaderOk, &topology);

        assert!(
            matches!(state, DiskState::Offline),
            "expected Offline, got {state:?}"
        );
    }

    // Intent: live-pool reconciliation keeps verified members healthy when
    //   the assembled UUID set contains them.
    // Why it exists: the new cross-check must not create false warnings for
    //   fully assembled pools.
    // Scenario: a healthy mounted pool contains the declared member's LUKS
    //   UUID in the live btrfs device set.
    #[test]
    fn reconcile_keeps_verified_member_ok_when_present_in_live_pool() {
        let uuid = test_uuid(1);
        let topology = LiveTopology::Online(HashSet::from([uuid.clone()]));

        let state = reconcile_with_live_pool(&uuid, DiskState::LuksHeaderOk, &topology);

        assert!(
            matches!(state, DiskState::LuksHeaderOk),
            "expected LuksHeaderOk, got {state:?}"
        );
    }

    // Intent: live-pool reconciliation preserves identity-only behavior when
    //   the pool is offline.
    // Why it exists: declared_disks must keep its existing offline-pool
    //   behavior and not invent btrfs membership findings without a mount.
    // Scenario: all raw declared disks are present while the NAS pool is not
    //   mounted.
    #[test]
    fn reconcile_keeps_state_when_pool_offline() {
        let uuid = test_uuid(1);
        let topology = LiveTopology::Offline;

        let state = reconcile_with_live_pool(&uuid, DiskState::LuksHeaderOk, &topology);

        assert!(
            matches!(state, DiskState::LuksHeaderOk),
            "expected LuksHeaderOk, got {state:?}"
        );
    }

    // Intent: live-pool reconciliation does not fabricate offline rows when
    //   topology probing fails.
    // Why it exists: a probe error is a check-level indeterminate warning, not
    //   evidence that any specific member is absent.
    // Scenario: btrfs probing fails while a declared member's LUKS identity
    //   still verifies.
    #[test]
    fn reconcile_unavailable_topology_does_not_fabricate_offline() {
        let uuid = test_uuid(1);
        let topology = LiveTopology::Unavailable("boom".into());

        let state = reconcile_with_live_pool(&uuid, DiskState::LuksHeaderOk, &topology);

        assert!(
            matches!(state, DiskState::LuksHeaderOk),
            "expected LuksHeaderOk, got {state:?}"
        );
    }

    // Intent: live-pool reconciliation only upgrades verified members and
    //   never masks stronger per-disk problems.
    // Why it exists: missing disks and UUID mismatches have their own
    //   remediation and severity paths that offline must not hide.
    // Scenario: a degraded live pool is missing a UUID while a declared disk
    //   is already classified missing or mismatched.
    #[test]
    fn reconcile_never_masks_real_problem() {
        let uuid = test_uuid(1);
        let observed = test_uuid(2);
        let topology = LiveTopology::Online(HashSet::new());

        let missing = reconcile_with_live_pool(&uuid, DiskState::Missing, &topology);
        assert!(
            matches!(missing, DiskState::Missing),
            "expected Missing, got {missing:?}"
        );

        let mismatch = reconcile_with_live_pool(
            &uuid,
            DiskState::LuksUuidMismatch {
                expected: uuid.clone(),
                observed: observed.clone(),
            },
            &topology,
        );
        match mismatch {
            DiskState::LuksUuidMismatch {
                expected,
                observed: got_observed,
            } => {
                assert_eq!(expected.as_str(), uuid.as_str());
                assert_eq!(got_observed.as_str(), observed.as_str());
            }
            other => panic!("expected LuksUuidMismatch, got {other:?}"),
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

        let result = summarize_declared_disks(&inputs, None);

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

    // Intent: when a UUID mismatch coexists with any other warn-level problem,
    //   the declared_disks check still reports Fail.
    // Why it exists: the severity rule must remain "Fail iff uuid_mismatch is
    //   non-empty"; pairing the mismatch only with a healthy disk would miss a
    //   regression that makes mismatch fail only when it is the sole problem.
    // Scenario: a degraded NAS has one swapped declared disk and another
    //   classified LuksHeaderUnreadable before any mutating command runs.
    #[test]
    fn summarize_declared_disks_fail_dominates_warn_level_problems() {
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
            cls(
                "disk2",
                "/dev/disk/by-id/wwn-0x2",
                DiskState::LuksHeaderUnreadable,
            ),
        ];

        let result = summarize_declared_disks(&inputs, None);

        assert_eq!(result.status, CheckStatus::Fail);
        let msg = &result.message;
        assert!(msg.contains("disk1"), "missing disk1: {msg}");
        assert!(msg.contains("disk2"), "missing disk2: {msg}");
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
        let result = summarize_declared_disks(&inputs, None);
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
         * Scenario: a degraded NAS with one missing disk, one classified
         *   LuksHeaderUnreadable, and one with a probe failure
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
                DiskState::ProbeFailed("simulated probe failure".to_owned()),
            ),
            cls("disk4", "/dev/disk/by-id/wwn-0x4", DiskState::LuksHeaderOk),
        ];
        let result = summarize_declared_disks(&inputs, None);
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
    //   operation (docs/design/principles.md#3-safe-by-construction-operations)
    //   and tests/repro/degraded-soft-balance.py.
    //   The mixed-profile warning's balance suggestion contradicts that order on a
    //   degraded pool; this test pins the routing that keeps the two messages aligned.
    // Scenario: a 2-disk RAID1 lost a disk; new chunks were allocated as `single`
    //   while degraded. doctor reports the mixed profile and must tell the operator
    //   to replace before balancing.
    #[test]
    fn data_profile_mismatch_recommends_replace_when_degraded() {
        let (df_req, df_out) = df_json(DF_MIXED);
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2])
            .with_output(df_req, df_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
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
    //   that test exercises the Err fallback by leaving BtrfsFilesystemShow unmocked.
    // Scenario: operator interrupted a balance midway; mixed profiles exist but
    //   all members are present. doctor should still recommend the balance.
    #[test]
    fn data_profile_mismatch_recommends_balance_when_healthy() {
        let (df_req, df_out) = df_json(DF_MIXED);
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[])
            .with_output(df_req, df_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
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

    // --- paused_balance tests ---

    fn paused_balance_expected_message() -> &'static str {
        "paused balance detected -- will not auto-resume; run: btrfs balance resume /mnt/storage"
    }

    fn paused_balance_check_result(runner: MockRunner) -> CheckResult {
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        check_paused_balance(&mut ctx)
    }

    // Intent: paused_balance warns with resume guidance for a mounted pool.
    // Why it exists: doctor is the recurring diagnostic surface, so a paused
    //   btrfs balance must not rely only on unlock's one-shot warning.
    // Scenario: an operator paused a balance after real chunk progress and
    //   later runs `braid doctor` while the pool remains mounted.
    #[test]
    fn paused_balance_warns_with_resume_hint() {
        let mp = MountPoint("/mnt/storage".to_owned());
        let (mp_req, mp_out) = mountpoint_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_paused(&mp);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(balance_req, balance_out);

        let check = paused_balance_check_result(runner);

        assert_eq!(check.name, "paused_balance");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.message, paused_balance_expected_message());
        assert!(
            !check.message.contains('%'),
            "paused balance advice must not print progress: {}",
            check.message
        );
        assert!(
            !check.message.contains("chunks"),
            "paused balance advice must not print chunks: {}",
            check.message
        );
    }

    // Intent: paused_balance ignores btrfs's `nan% left` paused fixture.
    // Why it exists: skip_balance remounts can report 0/0 chunks with nan%,
    //   and doctor must not render that as misleading completion progress.
    // Scenario: a reboot remounts with skip_balance after an interrupted
    //   balance, then the operator checks doctor before resuming it.
    #[test]
    fn paused_balance_skip_balance_nan_warns_without_progress() {
        let mp = MountPoint("/mnt/storage".to_owned());
        let (mp_req, mp_out) = mountpoint_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_paused_skip_balance(&mp);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(balance_req, balance_out);

        let check = paused_balance_check_result(runner);

        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.message, paused_balance_expected_message());
        assert!(
            !check.message.contains("100% complete"),
            "nan% fixture must not become completion advice: {}",
            check.message
        );
    }

    // Intent: paused_balance reports Ok when btrfs says no balance is paused.
    // Why it exists: adding the doctor check must stay quiet for the normal
    //   mounted-pool state.
    // Scenario: a healthy mounted pool has no active or paused balance.
    #[test]
    fn paused_balance_idle_ok() {
        let mp = MountPoint("/mnt/storage".to_owned());
        let (mp_req, mp_out) = mountpoint_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(balance_req, balance_out);

        let check = paused_balance_check_result(runner);

        assert_eq!(check.name, "paused_balance");
        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.message, "no paused balance");
    }

    // Intent: paused_balance warns when the mounted-pool balance probe fails.
    // Why it exists: doctor should not report green when it cannot determine
    //   whether a mounted pool is mid-balance.
    // Scenario: `btrfs balance status` cannot be spawned or queried, but the
    //   mountpoint itself is present.
    #[test]
    fn paused_balance_warns_when_status_probe_errors() {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_handler(|request| match request {
                CmdRequest::BtrfsBalanceStatus { .. } => Some(Err(CmdError::Failed(
                    "simulated balance status failure".to_owned(),
                ))),
                _ => None,
            });

        let check = paused_balance_check_result(runner);

        assert_eq!(check.name, "paused_balance");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.message, "could not inspect balance status");
    }

    // Intent: paused_balance skips when the configured pool is not mounted.
    // Why it exists: querying balance status requires a mounted btrfs pool.
    // Scenario: the NAS is booted but `braid unlock` has not mounted storage.
    #[test]
    fn paused_balance_skips_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);

        let check = paused_balance_check_result(runner);

        assert_eq!(check.name, "paused_balance");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (pool not mounted)");
    }

    // Intent: paused_balance skips when doctor has no parsed config.
    // Why it exists: without config, doctor has no authoritative mountpoint
    //   to query.
    // Scenario: config loading fails before mounted-pool checks run.
    #[test]
    fn paused_balance_skips_when_config_unavailable() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = beep_ctx(&runner, &paths);

        let check = check_paused_balance(&mut ctx);

        assert_eq!(check.name, "paused_balance");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (config not available)");
    }

    // Intent: run_doctor registers paused_balance and human formatting labels it.
    // Why it exists: direct check tests cannot catch forgetting to add the
    //   check to the run list or formatter label table.
    // Scenario: a full doctor run observes a paused balance on a mounted pool.
    #[test]
    fn run_doctor_reports_paused_balance_with_human_label() {
        let mp = MountPoint("/mnt/storage".to_owned());
        let (mp_req, mp_out) = mountpoint_ok();
        let (balance_req, balance_out) = unlock_btrfs_balance_status_paused(&mp);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(balance_req, balance_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );

        let check = find_check(&report, "paused_balance");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.message, paused_balance_expected_message());
        let human = format_doctor_human(&report);
        assert!(
            human.contains("paused balance"),
            "expected human paused-balance label:\n{human}"
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
        let (df_req, df_out) = df_json(DF_MIXED_METADATA);
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2])
            .with_output(df_req, df_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
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

    // Intent: system_profile_mismatch reports Ok for uniform RAID1 system chunks.
    // Why it exists: System is now first-class in the status Profile section
    // and doctor must cover its healthy baseline too.
    // Scenario: a clean RAID1 pool has System block groups on RAID1.
    #[test]
    fn system_profile_clean_raid1_ok() {
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

        let check = find_check(&report, "system_profile_mismatch");

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("RAID1"),
            "expected RAID1 in message: {}",
            check.message
        );
    }

    // Intent: system_profile_mismatch warns when System block groups span
    // multiple profiles.
    // Why it exists: a System row that status renders as not fully redundant
    // must route to doctor for the same soft-balance guidance as metadata.
    // Scenario: an interrupted metadata/system balance leaves both DUP and RAID1 system chunks.
    #[test]
    fn system_profile_mixed_warns() {
        let mixed_system = r#"{
            "filesystem-df": [
                { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
                { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
                { "bg-type": "System", "bg-profile": "DUP", "total": 8388608, "used": 16384 },
                { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 }
            ]
        }"#;
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(mixed_system);
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

        let check = find_check(&report, "system_profile_mismatch");

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("mixed"),
            "expected mixed-profile warning: {}",
            check.message
        );
        assert!(
            check.message.contains("-mconvert=raid1,soft"),
            "expected soft metadata/system balance suggestion: {}",
            check.message
        );
    }

    // Intent: system_profile_mismatch recommends replace before balance on a
    // degraded pool.
    // Why it exists: the status System row tells operators to run doctor, so
    // doctor must preserve the replace-first invariant for that row.
    // Scenario: degraded operation allocated non-RAID1 system chunks while a member was missing.
    #[test]
    fn system_profile_mismatch_recommends_replace_when_degraded() {
        let mixed_system = r#"{
            "filesystem-df": [
                { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
                { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 262144 },
                { "bg-type": "System", "bg-profile": "DUP", "total": 8388608, "used": 16384 },
                { "bg-type": "System", "bg-profile": "RAID1", "total": 8388608, "used": 16384 }
            ]
        }"#;
        let (df_req, df_out) = df_json(mixed_system);
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2])
            .with_output(df_req, df_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());

        let check = find_check(&report, "system_profile_mismatch");

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

    // --- enospc_risk tests ---

    // Intent: enospc_risk reports Ok when a 3-device pool has enough RAID1
    //   chunk-pair headroom after any single disk loss.
    // Why it exists: the proactive risk row must not add noise to healthy
    //   doctor output.
    // Scenario: three 100 GiB devices each have 5 GiB unallocated.
    #[test]
    fn enospc_risk_healthy_three_disk_pool_ok() {
        let usage = enospc_device_usage(&[5 * GIB, 5 * GIB, 5 * GIB], 100 * GIB);
        let runner = enospc_three_disk_runner(&usage);
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let check = check_enospc_risk(&mut ctx);

        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.message, "per-device unallocated space healthy");
    }

    // Intent: enospc_risk warns with the shared status advisory wording.
    // Why it exists: status and doctor should stay in lockstep for the same
    //   RAID1 chunk-pair risk predicate.
    // Scenario: three 100 GiB devices have unallocated [10 GiB, 10 GiB, 50 MiB].
    #[test]
    fn enospc_risk_low_unallocated_warns() {
        let usage = enospc_device_usage(&[10 * GIB, 10 * GIB, 50 * MIB], 100 * GIB);
        let runner = enospc_three_disk_runner(&usage);
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let check = check_enospc_risk(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.starts_with("ENOSPC risk:"),
            "expected ENOSPC advisory: {}",
            check.message
        );
    }

    // Intent: enospc_risk fails loud when btrfs device usage cannot be read.
    // Why it exists: an unavailable risk input should be visible instead of a
    //   false healthy row.
    // Scenario: pool state probes succeed but `btrfs device usage` is missing.
    #[test]
    fn enospc_risk_device_usage_failure_warns() {
        let runner = pool_state_runner(
            vec![
                ("braid-disk1", 1, "/dev/vdb", test_uuid(1)),
                ("braid-disk2", 2, "/dev/vdc", test_uuid(2)),
            ],
            &[],
        );
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let check = check_enospc_risk(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(
            check.message,
            "btrfs device usage failed -- ENOSPC risk indeterminate"
        );
    }

    // Intent: enospc_risk fails loud when live pool state cannot be probed.
    // Why it exists: missing-count uncertainty controls whether the risk row
    //   should skip degraded pools, so probe failure must not become Ok.
    // Scenario: mountpoint succeeds but `btrfs filesystem show` is unavailable.
    #[test]
    fn enospc_risk_pool_state_failure_warns() {
        let runner = DfQueryFailureRunner;
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let check = check_enospc_risk(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .starts_with("could not probe pool state -- ENOSPC risk indeterminate:"),
            "expected pool-state warning: {}",
            check.message
        );
    }

    // Intent: enospc_risk skips degraded pools.
    // Why it exists: the missing-device row is the louder signal once the pool
    //   has already lost a member.
    // Scenario: btrfs reports one MISSING devid in a two-device pool.
    #[test]
    fn enospc_risk_degraded_pool_skips() {
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2]);
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

        let check = check_enospc_risk(&mut ctx);

        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (pool is degraded)");
    }

    // --- metadata_enospc_pressure tests ---

    fn metadata_pressure_result(df: &str, usage: impl AsRef<str>) -> CheckResult {
        let usage = usage.as_ref();
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(df);
        let (usage_req, usage_out) = device_usage_raw(usage);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(usage_req, usage_out);
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        check_metadata_enospc_pressure(&mut ctx)
    }

    fn metadata_pressure_result_with_pool(
        df: &str,
        usage: impl AsRef<str>,
        present: Vec<(&'static str, u64, &'static str, LuksUuid)>,
        missing_devids: &[u64],
    ) -> CheckResult {
        let usage = usage.as_ref();
        let (df_req, df_out) = df_json(df);
        let (usage_req, usage_out) = device_usage_raw(usage);
        let runner = pool_state_runner(present, missing_devids)
            .with_output(df_req, df_out)
            .with_output(usage_req, usage_out);
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());
        check_metadata_enospc_pressure(&mut ctx)
    }

    fn metadata_pressure_with_cached_pool_state(
        df: &str,
        usage: impl AsRef<str>,
        pool_state: Result<PoolState, ProbeError>,
    ) -> CheckResult {
        let usage = usage.as_ref();
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(df);
        let (usage_req, usage_out) = device_usage_raw(usage);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(usage_req, usage_out);
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        ctx.pool_state = Some(pool_state);
        check_metadata_enospc_pressure(&mut ctx)
    }

    // Intent: metadata_enospc_pressure reports Ok for a healthy RAID1 pool.
    // Why it exists: the advisory must not add noise to ordinary doctor output.
    // Scenario: a pool has low metadata utilization and both devices have room
    //   for future metadata chunk allocation.
    #[test]
    fn metadata_pressure_healthy_pool_ok() {
        let check = metadata_pressure_result(DF_RAID1_CLEAN, device_usage_two_healthy());

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("within bounds"),
            "expected healthy message: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure ignores high metadata utilization when
    //   device headroom is still available.
    // Why it exists: the original audit's bare utilization threshold would
    //   warn on healthy pools immediately before normal chunk growth.
    // Scenario: metadata is 78% used, but both RAID1 devices have multiple GiB
    //   unallocated for the allocator's next metadata chunk.
    #[test]
    fn metadata_pressure_high_metadata_with_headroom_ok() {
        let check = metadata_pressure_result(DF_METADATA_78_USED, device_usage_two_healthy());

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("within bounds"),
            "expected no warning with headroom: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure ignores low unallocated space when
    //   metadata utilization is still low.
    // Why it exists: the advisory targets the ENOSPC trap where metadata must
    //   grow and cannot, not every nearly allocated device.
    // Scenario: each device has less than 1 GiB unallocated, but metadata is
    //   only 20% used and does not need a new chunk soon.
    #[test]
    fn metadata_pressure_low_headroom_with_low_metadata_ok() {
        let check = metadata_pressure_result(DF_METADATA_20_USED, device_usage_two_tight());

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("within bounds"),
            "expected no warning with low metadata use: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure warns when high metadata utilization and
    //   exhausted per-device headroom coincide on a two-device pool.
    // Why it exists: this is the actual btrfs metadata ENOSPC trap the check is
    //   meant to surface before writes force the filesystem read-only.
    // Scenario: metadata is 78% used and neither RAID1 member has enough
    //   unallocated space for the next metadata chunk.
    #[test]
    fn metadata_pressure_two_device_pool_warns_when_both_signals_present() {
        let check = metadata_pressure_result_with_pool(
            DF_METADATA_78_USED,
            device_usage_two_tight(),
            vec![
                ("braid-disk1", 1, "/dev/vdb", test_uuid(1)),
                ("braid-disk2", 2, "/dev/vdc", test_uuid(2)),
            ],
            &[],
        );

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("78%"), "{}", check.message);
        assert!(
            check.message.contains("RAID1 needs 2"),
            "missing RAID1 headroom requirement: {}",
            check.message
        );
        assert!(
            check.message.contains("btrfs balance start -dusage="),
            "missing data-balance remediation: {}",
            check.message
        );
        assert!(
            check.message.contains("delete files"),
            "missing metadata remediation: {}",
            check.message
        );
        assert!(
            !check.message.contains("mconvert") && !check.message.contains("musage"),
            "must not recommend metadata balancing: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure counts RAID1 allocator headroom, not
    //   every device in a 3-device pool.
    // Why it exists: RAID1 metadata chunks need two devices; a single tight
    //   member should not warn when two other members can satisfy allocation.
    // Scenario: metadata is 78% used, one device is tight, and two devices have
    //   multi-GiB unallocated.
    #[test]
    fn metadata_pressure_three_device_pool_one_tight_ok() {
        let check = metadata_pressure_result(DF_METADATA_78_USED, device_usage_three_one_tight());

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("within bounds"),
            "expected allocator-aware Ok: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure warns on the 3-device boundary where
    //   fewer than two devices have chunk headroom.
    // Why it exists: pins the allocator-aware "fewer than 2" rule instead of
    //   an all-devices or any-device heuristic.
    // Scenario: metadata is 78% used, two devices are tight, and only one
    //   device can participate in the next RAID1 metadata chunk.
    #[test]
    fn metadata_pressure_three_device_pool_two_tight_warns() {
        let check = metadata_pressure_result_with_pool(
            DF_METADATA_78_USED,
            device_usage_three_two_tight(),
            vec![
                ("braid-disk1", 1, "/dev/vdb", test_uuid(1)),
                ("braid-disk2", 2, "/dev/vdc", test_uuid(2)),
                ("braid-disk3", 3, "/dev/vdd", test_uuid(3)),
            ],
            &[],
        );

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("only 1 of 3"),
            "expected headroom count in warning: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure skips a degraded pool instead of
    //   recommending a data balance.
    // Why it exists: a balance on a degraded RAID1 pool allocates single-profile
    //   chunks and widens the recovery surface; braid's invariant is replace-first,
    //   then balance (docs/design/principles.md, 001-btrfs-raid1.md). Pins parity
    //   with check_enospc_risk's degraded skip.
    // Scenario: btrfs reports one MISSING devid while metadata is 78% used and
    //   both members are tight on unallocated space -- the exact state that would
    //   otherwise emit the `btrfs balance start -dusage=50` recommendation.
    #[test]
    fn metadata_pressure_degraded_pool_skips() {
        let check = metadata_pressure_result_with_pool(
            DF_METADATA_78_USED,
            device_usage_two_tight(),
            vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))],
            &[2],
        );

        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (pool is degraded)");
    }

    // Intent: metadata_enospc_pressure fails closed when pool state is
    //   indeterminate -- it must not emit the balance recommendation.
    // Why it exists: the degraded gate's whole point is to never recommend a
    //   degraded balance; if probing the pool fails we cannot confirm the pool is
    //   healthy, so the unsafe `btrfs balance start -dusage=50` text must be
    //   suppressed. The healthy/degraded tests would not catch a fall-through here.
    // Scenario: metadata is 78% used with both members tight (the warn condition),
    //   but the pool probe errored.
    #[test]
    fn metadata_pressure_indeterminate_pool_state_warns_without_balance() {
        let check = metadata_pressure_with_cached_pool_state(
            DF_METADATA_78_USED,
            device_usage_two_tight(),
            Err(ProbeError::PoolDevice {
                mapper: "braid-disk1".to_owned(),
                detail: "simulated probe failure".to_owned(),
            }),
        );

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("metadata pressure indeterminate"),
            "expected fail-closed probe warning: {}",
            check.message
        );
        assert!(
            !check.message.contains("btrfs balance start"),
            "must not recommend a balance when pool state is unknown: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure returns Ok on a degraded pool when there
    //   is no metadata pressure -- it does not early-skip like check_enospc_risk.
    // Why it exists: the gate is placed inside the warn condition by design
    //   (degraded-ness only matters when about to recommend a balance). An
    //   accidental move to an early degraded skip would flip this to Skip.
    // Scenario: one devid is MISSING, but metadata is only 20% used and both
    //   members have ample unallocated space -- nothing to warn about.
    #[test]
    fn metadata_pressure_degraded_but_no_pressure_returns_ok() {
        let check = metadata_pressure_with_cached_pool_state(
            DF_METADATA_20_USED,
            device_usage_two_healthy(),
            Ok(PoolState {
                mounted: true,
                devices: vec![],
                missing_count: 1,
                total_devices: 2,
                fsid: None,
                missing_devids: vec![2],
                null_underlying: vec![],
            }),
        );

        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("within bounds"),
            "degraded pool without pressure must report Ok: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure skips when the configured pool is not
    //   mounted.
    // Why it exists: the check is read-only and must not query btrfs commands
    //   against an absent mountpoint.
    // Scenario: the NAS has booted, but the encrypted pool has not been
    //   unlocked yet.
    #[test]
    fn metadata_pressure_skip_when_pool_not_mounted() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        let check = check_metadata_enospc_pressure(&mut ctx);

        assert_eq!(check.status, CheckStatus::Skip);
        assert!(check.message.contains("not mounted"), "{}", check.message);
    }

    // Intent: metadata_enospc_pressure reports df query failures as a scoped
    //   warning.
    // Why it exists: doctor should continue running and name the unavailable
    //   input instead of failing the whole command.
    // Scenario: the mountpoint exists, but `btrfs filesystem df` cannot be
    //   spawned or returns an unreadable result.
    #[test]
    fn metadata_pressure_warns_when_df_query_errors() {
        let runner = DfQueryFailureRunner;
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        let check = check_metadata_enospc_pressure(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("could not inspect metadata pressure"),
            "expected df inspect warning: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure reports malformed df JSON as a scoped
    //   warning.
    // Why it exists: the pressure math depends on parsed metadata totals, and
    //   parser drift must not become a false Ok.
    // Scenario: `btrfs filesystem df --format json` exits 0 but emits output
    //   that no longer matches braid's parser contract.
    #[test]
    fn metadata_pressure_warns_when_df_json_malformed() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json("{not json");
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        let check = check_metadata_enospc_pressure(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("could not inspect metadata pressure"),
            "expected df parse warning: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure reports device-usage spawn failures as
    //   a scoped warning.
    // Why it exists: the advisory depends on device unallocated bytes, but a
    //   missing secondary probe should not make doctor crash.
    // Scenario: df output parses, then the raw `btrfs device usage` probe
    //   cannot be run.
    #[test]
    fn metadata_pressure_warns_when_device_usage_query_errors() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_METADATA_78_USED);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out);
        let (_dir, paths) = isolated_paths();
        let mut ctx = parsed_doctor_ctx(&runner, &paths);
        let check = check_metadata_enospc_pressure(&mut ctx);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("could not inspect device unallocated"),
            "expected device usage warning: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure reports malformed device-usage output
    //   as a scoped warning.
    // Why it exists: parser drift in btrfs-progs must degrade to an advisory
    //   row, not a panic or a false Ok.
    // Scenario: `btrfs device usage --raw` exits 0 but omits the required
    //   Unallocated field from a device stanza.
    #[test]
    fn metadata_pressure_warns_when_device_usage_parse_fails() {
        let malformed = "/dev/mapper/braid-disk1, ID: 1\n\
                         \x20  Device size:          10737418240\n\
                         \x20  Device slack:         0\n";
        let check = metadata_pressure_result(DF_METADATA_78_USED, malformed);

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("could not inspect device unallocated")
                && check.message.contains("could not parse"),
            "expected parse warning: {}",
            check.message
        );
    }

    // Intent: metadata_enospc_pressure treats an empty parsed device list as
    //   an inspection warning.
    // Why it exists: parse_btrfs_device_usage accepts empty stdout, but this
    //   check cannot reduce zero devices into an allocator headroom decision.
    // Scenario: `btrfs device usage --raw` exits 0 with no device stanzas.
    #[test]
    fn metadata_pressure_warns_when_device_usage_reports_no_devices() {
        let check = metadata_pressure_result(DF_METADATA_78_USED, "");

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("no devices reported"),
            "expected empty-device warning: {}",
            check.message
        );
    }

    // Intent: run_doctor registers metadata_enospc_pressure and the human
    //   formatter labels it as "meta pressure".
    // Why it exists: the JSON check name and the operator-facing label are
    //   separate surfaces and both must stay wired into the doctor report.
    // Scenario: operator runs `braid doctor` on a healthy mounted pool.
    #[test]
    fn metadata_pressure_registered_with_human_label() {
        let (mp_req, mp_out) = mountpoint_ok();
        let (df_req, df_out) = df_json(DF_RAID1_CLEAN);
        let usage = device_usage_two_healthy();
        let (usage_req, usage_out) = device_usage_raw(&usage);
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(df_req, df_out)
            .with_output(usage_req, usage_out);
        let f = write_temp(valid_config_json());
        let report = run_doctor(
            f.path(),
            &runner,
            &RealFilesystem,
            &isolated_paths().1,
            human_options(),
        );

        let check = find_check(&report, "metadata_enospc_pressure");
        assert_eq!(check.status, CheckStatus::Ok);
        let human = format_doctor_human(&report);
        assert!(
            human.contains("meta pressure"),
            "expected 'meta pressure':\n{human}"
        );
    }

    // --- pool_missing_devices tests ---

    // Intent: pool_missing_devices reports Ok when no devices are missing.
    // Why: ensures the check doesn't false-positive on a healthy pool.
    // Scenario: healthy 1-disk pool, all present.
    #[test]
    fn pool_missing_devices_ok_when_healthy() {
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
        let check = find_check(&report, "pool_missing_devices");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.message.contains("no missing"), "{}", check.message);
    }

    /* Intent: pool_missing_devices can run without querying `btrfs filesystem df`.
     * Why it exists: missing-device detection now reads `pool_state.missing_devids`
     * (sourced from `BtrfsFilesystemShow` via `probe::probe_pool`); tying the
     * check to df would make an unrelated parser or command failure hide the
     * more specific live-pool probe.
     * Scenario: the pool is mounted and healthy, while the df command would fail
     * if this check accidentally requested it.
     */
    #[test]
    fn pool_missing_devices_does_not_require_filesystem_df() {
        let runner = PoolMissingDevicesRunner::default();
        let (_dir, paths) = isolated_paths();
        let fs = DoctorMockFs::mounted_btrfs_only();
        let mut ctx =
            DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

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
                .any(|c| matches!(c, CmdRequest::BtrfsFilesystemShow { .. })),
            "expected btrfs filesystem show probe, got: {calls:?}"
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
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
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
            check.message.contains(
                "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
            ),
            "expected full replace recommendation: {}",
            check.message
        );
        assert!(
            !check.message.contains("braid replace --missing-id"),
            "replace recommendation must not render bare --missing-id command: {}",
            check.message
        );
        assert!(
            check.message.contains("devid"),
            "expected devid in message: {}",
            check.message
        );
    }

    // Intent: pool_missing_devices plural output lists missing devids once and
    // shows a single base replace command plus optional cross-check wording.
    // Why it exists: multi-missing guidance should not print one command per
    // devid or regress to bare `replace --missing-id` instructions.
    // Scenario: two pool members are missing and doctor guides the operator to
    // replace one by name while optionally checking against the listed devids.
    #[test]
    fn pool_missing_devices_plural_warns_with_single_replace_command() {
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[2, 3]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
        let check = find_check(&report, "pool_missing_devices");

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .contains("pool has 2 missing devices (devids: 2, 3)"),
            "expected plural devid list: {}",
            check.message
        );
        assert_eq!(
            check.message.matches("braid replace --old").count(),
            1,
            "expected exactly one replace command: {}",
            check.message
        );
        assert!(
            check.message.contains(
                "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
            ),
            "expected base replace command: {}",
            check.message
        );
        assert!(
            check
                .message
                .contains("Optionally add `--missing-id <devid>` as a cross-check."),
            "expected optional cross-check phrase: {}",
            check.message
        );
        assert!(
            check.message.contains("Use one of the listed IDs."),
            "expected multi-missing cross-check target: {}",
            check.message
        );
        assert!(
            !check.message.contains("braid replace --missing-id"),
            "must not render bare replace --missing-id guidance: {}",
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
        let runner = pool_state_runner(
            vec![
                ("braid-disk1", 1, "/dev/vdb", known_uuid),
                ("braid-stranger", 2, "/dev/vdc", foreign_uuid.clone()),
            ],
            &[],
        );
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
        assert!(
            check
                .message
                .contains("btrfs device remove /dev/mapper/braid-stranger"),
            "remove command must name the concrete mapper: {}",
            check.message
        );
        assert!(
            check.message.contains("cryptsetup close braid-stranger"),
            "close command must name the concrete mapper: {}",
            check.message
        );
        assert!(
            !check.message.contains("<mapper>"),
            "remediation must not leak the <mapper> placeholder: {}",
            check.message
        );
    }

    // Intent: foreign_luks_uuid pairs every foreign mapper with its own
    //   concrete remove+close recipe (not a shared <mapper> placeholder), and
    //   pluralizes the count, when the live pool admits more than one unknown
    //   LUKS UUID.
    // Why it exists: the remediation runs in a high-stakes manual recovery, so
    //   each foreign mapper must yield a paste-ready command; a single shared
    //   <mapper> clause forces the operator to hand-substitute every name.
    // Scenario: an operator force-adds two independently formatted LUKS mappers
    //   (braid-stranger, braid-other) into the live pool outside braid.
    #[test]
    fn check_foreign_luks_uuid_emits_concrete_command_per_foreign_mapper() {
        let (_dir, paths) = isolated_paths();
        save_doctor_membership(
            &paths,
            &[(180, "disk1", "/dev/disk/by-id/virtio-disk1", Some(1))],
        );
        let known_uuid = test_uuid(180);
        let stranger_uuid = test_uuid(181);
        let other_uuid = test_uuid(182);
        let runner = pool_state_runner(
            vec![
                ("braid-disk1", 1, "/dev/vdb", known_uuid),
                ("braid-stranger", 2, "/dev/vdc", stranger_uuid.clone()),
                ("braid-other", 3, "/dev/vdd", other_uuid.clone()),
            ],
            &[],
        );
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &paths, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(report.status, CheckStatus::Fail);
        // Trailing `s` matters: "2 foreign LUKS UUID" is a prefix of the plural
        // and would pass even if pluralization regressed.
        assert!(
            check.message.contains("2 foreign LUKS UUIDs"),
            "expected pluralized count: {}",
            check.message
        );
        for needle in [stranger_uuid.as_str(), other_uuid.as_str()] {
            assert!(
                check.message.contains(needle),
                "missing foreign UUID {needle:?} in: {}",
                check.message
            );
        }
        // Iteration is by LuksUuid (BTreeMap key), so assert per-mapper
        // substring presence and per-mapper ordering rather than positional
        // order across mappers.
        for mapper in ["braid-stranger", "braid-other"] {
            let remove = format!("btrfs device remove /dev/mapper/{mapper}");
            let close = format!("cryptsetup close {mapper}");
            let remove_pos = check.message.find(&remove).unwrap_or_else(|| {
                panic!("missing concrete remove for {mapper}: {}", check.message)
            });
            let close_pos = check.message.find(&close).unwrap_or_else(|| {
                panic!("missing concrete close for {mapper}: {}", check.message)
            });
            assert!(
                remove_pos < close_pos,
                "{mapper}: remove must precede close: {}",
                check.message
            );
        }
        assert!(
            !check.message.contains("<mapper>"),
            "remediation must not leak the <mapper> placeholder: {}",
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
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", known_uuid)], &[]);
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

    // Intent: foreign_luks_uuid treats an empty pool.json as no declared
    //   members instead of classifying all live pool UUIDs as foreign.
    // Why it exists: an empty membership should be a setup-state Skip, not a
    //   spurious foreign-UUID failure.
    // Scenario: the pool is mounted and pool.json parses with zero disks.
    #[test]
    fn foreign_luks_uuid_skips_when_membership_is_empty() {
        let (_dir, paths) = isolated_paths();
        membership::save_membership(&membership::PoolMembership::empty(), &paths)
            .expect("empty membership saves");
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &paths, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Skip);
        assert_eq!(check.message, "skipped (no pool members declared)");
    }

    // Intent: foreign_luks_uuid warns on corrupt pool.json only after the pool
    //   is confirmed mounted.
    // Why it exists: corrupt membership is actionable only when this mounted
    //   topology check can otherwise run.
    // Scenario: the pool is mounted but pool.json does not match the
    //   PoolMembership schema.
    #[test]
    fn foreign_luks_uuid_warns_on_corrupt_membership() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.pool_json(), "{}").expect("corrupt membership writes");
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default().with_output(mp_req, mp_out);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());

        let report = run_doctor(f.path(), &runner, &fs, &paths, human_options());

        let check = find_check(&report, "foreign_luks_uuid");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("could not load pool membership"),
            "{}",
            check.message
        );
    }

    // Intent: human format includes the "missing devs" label.
    // Why: ensures the new check has a human-readable label.
    // Scenario: operator reads braid doctor output.
    #[test]
    fn human_format_contains_missing_devs_label() {
        let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[]);
        let fs = DoctorMockFs::mounted_btrfs_only();
        let f = write_temp(valid_config_json());
        let report = run_doctor(f.path(), &runner, &fs, &isolated_paths().1, human_options());
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

    // Intent: unknown fields in notifier-config.json produce Fail through the
    //   existing malformed-config arm, not silent Skip or Ok.
    // Why it exists: NotifierConfig promises stale parsers cannot silently
    //   degrade. Without this test, dropping deny_unknown_fields would only
    //   surface after the module side added a field and production skew
    //   already existed.
    // Scenario: a future modules/braid/monitor.nix adds a webhook_url field to
    //   notifier-config.json against a CLI binary that predates the addition.
    #[test]
    fn beep_path_fail_on_unknown_field() {
        let f = write_temp(
            r#"{"beep_probe_path":"/run/current-system/sw/bin/beep","webhook_url":"https://example.invalid"}"#,
        );
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
    //   if an internal caller also sets the beep option.
    // Why: `braid doctor --json` is for programmatic consumption -- emitting
    //   an audible side effect from a data-output command would surprise
    //   any script piping doctor's JSON into a monitoring system. The public
    //   CLI rejects `--json --beep`; this lower-level guard is
    //   defense-in-depth. The runner-not-invoked invariant is enforced
    //   implicitly: MockRunner returns MissingMock for any unmatched call, so
    //   a regression that spawned the wrapper before checking the json gate
    //   would surface as a Fail rather than a Skip.
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
                beep: true, // Internal defense-in-depth despite CLI conflict.
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
    // Wake-on-LAN doctor checks

    fn config_with_auto_suspend() -> &'static str {
        r#"{"mount_point":"/mnt/storage","auto_suspend":{"wol_interface":"eno1"}}"#
    }

    // Intent: summarize_wol reports Ok when ethtool shows magic-packet wake armed.
    // Why it exists: this is the only green path for auto-suspend hosts; if it
    // drifts, doctor either strands a NAS silently or creates false failures.
    // Scenario: operator rebuilt with braid.autoSuspend.wolInterface and the
    // NIC reports Wake-on: g at runtime.
    #[test]
    fn wol_summary_ok_when_magic_packet_armed() {
        let r = summarize_wol(
            "eno1",
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n",
            "",
            0,
        );
        assert_eq!(r.status, CheckStatus::Ok, "got: {r:?}");
        assert!(r.message.contains("Wake-on: g"), "got: {}", r.message);
    }

    // Intent: summarize_wol fails when magic-packet wake is supported but off.
    // Why it exists: Wake-on: d is the dangerous runtime drift that can let
    // autosuspend make the NAS unreachable until physical access.
    // Scenario: BIOS ErP, a driver reset, or a missed rebuild leaves WoL disabled.
    #[test]
    fn wol_summary_fails_when_magic_packet_supported_but_disabled() {
        let r = summarize_wol(
            "eno1",
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: d\n",
            "",
            0,
        );
        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(
            r.message.contains("supports magic-packet WoL"),
            "got: {}",
            r.message
        );
        assert!(r.message.contains("Wake-on: d"), "got: {}", r.message);
    }

    // Intent: summarize_wol fails when the NIC/driver lacks magic-packet support.
    // Why it exists: braid.autoSuspend cannot be safe on a configured interface
    // whose driver reports no `g` support, regardless of NixOS option state.
    // Scenario: operator selects the wrong interface or a NIC without WoL support.
    #[test]
    fn wol_summary_fails_when_magic_packet_unsupported() {
        let r = summarize_wol(
            "eno1",
            "Settings for eno1:\n\tSupports Wake-on: d\n\tWake-on: d\n",
            "",
            0,
        );
        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(
            r.message
                .contains("does not report magic-packet WoL support"),
            "got: {}",
            r.message
        );
    }

    // Intent: summarize_wol fails when ethtool itself returns non-zero.
    // Why it exists: interface removal, EPERM, and driver errors all mean
    // doctor cannot prove the wake path, so the check must fail closed.
    // Scenario: braid.autoSuspend.wolInterface names an interface that no
    // longer exists after a NIC rename.
    #[test]
    fn wol_summary_fails_when_ethtool_query_fails() {
        let r = summarize_wol(
            "eno1",
            "",
            "Cannot get device wake-on-lan settings: No such device\n",
            1,
        );
        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(r.message.contains("exit 1"), "got: {}", r.message);
        assert!(r.message.contains("No such device"), "got: {}", r.message);
    }

    // Intent: summarize_wol fails closed when ethtool output is missing or drifted.
    // Why it exists: parser drift must never silently downgrade to Ok,
    // disabled, or unsupported because all three imply different operator action.
    // Scenario: a future ethtool changes the WoL labels or emits an unexpected
    // mode token.
    #[test]
    fn wol_summary_fails_closed_on_unparseable_ethtool_output() {
        for stdout in [
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n",
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: garbage\n",
        ] {
            let r = summarize_wol("eno1", stdout, "", 0);
            assert_eq!(r.status, CheckStatus::Fail, "stdout={stdout:?}, got: {r:?}");
            assert!(
                r.message.contains("could not parse ethtool output"),
                "got: {}",
                r.message
            );
        }
    }

    // Intent: check_wake_on_lan skips when auto_suspend is absent from config.
    // Why it exists: always-on systems should not see Wake-on-LAN-colored
    // diagnostics or require ethtool in standalone test configs.
    // Scenario: non-auto-suspend deployment runs `braid doctor`.
    #[test]
    fn wake_on_lan_check_skips_when_auto_suspend_absent() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = DoctorContext::for_test_parsed(&runner, &paths, valid_config_json());

        let r = check_wake_on_lan(&mut ctx);

        assert_eq!(r.status, CheckStatus::Skip, "got: {r:?}");
        assert!(
            r.message.contains("braid.autoSuspend not enabled"),
            "got: {}",
            r.message
        );
        assert!(runner.requests().is_empty(), "ethtool should not run");
    }

    // Intent: check_wake_on_lan fails when ethtool cannot be invoked.
    // Why it exists: missing wrapper wiring for braid.packages.ethtool would
    // otherwise hide the runtime wake-path check on exactly the hosts that need it.
    // Scenario: deployed wrapper omits ethtool from PATH.
    #[test]
    fn wake_on_lan_check_fails_when_ethtool_spawn_fails() {
        let runner = MockRunner::default().with_handler(|request| match request {
            CmdRequest::EthtoolShow { interface } if interface == "eno1" => Some(Err(
                CmdError::Failed("ethtool eno1: No such file or directory".into()),
            )),
            _ => None,
        });
        let (_dir, paths) = isolated_paths();
        let mut ctx = DoctorContext::for_test_parsed(&runner, &paths, config_with_auto_suspend());

        let r = check_wake_on_lan(&mut ctx);

        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(
            r.message.contains("ethtool invocation failed"),
            "got: {}",
            r.message
        );
        assert!(
            r.message.contains("braid.packages.ethtool"),
            "got: {}",
            r.message
        );
    }

    // Intent: run_doctor registers wake_on_lan and human formatting labels it.
    // Why it exists: direct classifier tests cannot catch forgetting to add the
    // check to the run list or formatter label table.
    // Scenario: operator runs `braid doctor` on an auto-suspend host.
    #[test]
    fn wake_on_lan_registered_with_human_label() {
        let runner = MockRunner::default().with_output(
            CmdRequest::EthtoolShow {
                interface: "eno1".into(),
            },
            RawCommandOutput {
                cmd: "ethtool eno1".into(),
                stdout: "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let (_dir, paths) = isolated_paths();
        let f = write_temp(config_with_auto_suspend());
        let report = run_doctor(f.path(), &runner, &RealFilesystem, &paths, human_options());

        assert_eq!(find_check(&report, "wake_on_lan").status, CheckStatus::Ok);
        let human = format_doctor_human(&report);
        assert!(
            human.contains("wake-on-lan"),
            "expected human wake-on-lan label:\n{human}"
        );
    }

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

    // Intent: check_braid_online_active_when_mounted skips UPS-configured
    // configs that are not module-managed.
    // Why it exists: standalone CLI installs may configure UPS reads without
    // owning braid-online.service, so doctor must not probe that unit.
    // Scenario: hand-written config.json has ups but omits systemd_lifecycle.
    #[test]
    fn braid_online_check_skips_when_lifecycle_disabled() {
        let runner = MockRunner::default();
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(
            &runner,
            &paths,
            r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"}}"#,
        );

        let r = check_braid_online_active_when_mounted(&mut ctx);

        assert_eq!(r.status, CheckStatus::Skip);
        assert_eq!(
            r.message,
            "skipped (systemd_lifecycle not configured -- braid-online is not Rust-managed)"
        );
        assert!(
            !runner.requests().iter().any(|request| matches!(
                request,
                CmdRequest::SystemctlShowActiveState { unit }
                    if unit == "braid-online.service"
            )),
            "unexpected braid-online probe: {:?}",
            runner.requests()
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
     * Scenario: `mark_online` has just started braid-online.service and
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
        for status in [
            "deactivating",
            "failed",
            "maintenance",
            "unknown",
            "",
            "bogus",
        ] {
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
            if status == "maintenance" {
                assert!(
                    r.message.contains("braid-online.service is maintenance"),
                    "expected known-state Fail wording, got: {}",
                    r.message
                );
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

    // Intent: check_braid_online_active_when_mounted reports a diagnostic
    // Fail when systemctl show exits non-zero.
    // Why it exists: ignoring the exit status used to collapse an absent or
    // masked unit into an empty ActiveState message with no operator clue.
    // Scenario: braid-online.service is not loaded on a UPS-enabled host while
    // the pool is mounted.
    #[test]
    fn braid_online_check_fails_with_systemctl_show_error() {
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
                RawCommandOutput {
                    cmd: "systemctl show -P ActiveState braid-online.service".into(),
                    stdout: String::new(),
                    stderr: "Unit braid-online.service not loaded.".into(),
                    exit_status: 4,
                },
            );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());

        let r = check_braid_online_active_when_mounted(&mut ctx);

        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(r.message.contains("braid-online.service"), "{}", r.message);
        assert!(
            r.message.contains("UPS shutdown will not unmount the pool"),
            "{}",
            r.message
        );
        assert!(r.message.contains("exit 4"), "{}", r.message);
        assert!(
            r.message.contains("Unit braid-online.service not loaded."),
            "{}",
            r.message
        );
    }

    // Intent: check_braid_online_active_when_mounted fails when the mountpoint
    // probe itself errors.
    // Why it exists: per ADR 020, a mounted pool without braid-online active is
    // the highest-severity doctor finding, so an indeterminate mount probe must
    // not silently downgrade the UPS shutdown safety check to Skip.
    // Scenario: `mountpoint(1)` returns exit 1, such as permission denied while
    // resolving the path, and doctor cannot prove whether the pool is online.
    #[test]
    fn braid_online_check_fails_on_mountpoint_probe_error() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".into()),
            },
            RawCommandOutput {
                cmd: "mountpoint".into(),
                stdout: String::new(),
                stderr: "permission denied".into(),
                exit_status: 1,
            },
        );
        let (_dir, paths) = isolated_paths();
        let mut ctx = ups_ctx(&runner, &paths, config_with_ups_enabled());

        let r = check_braid_online_active_when_mounted(&mut ctx);

        assert_eq!(r.status, CheckStatus::Fail, "got: {r:?}");
        assert!(
            r.message.contains("mountpoint probe"),
            "unexpected message: {}",
            r.message
        );
        assert!(
            r.message.contains("UPS shutdown"),
            "unexpected message: {}",
            r.message
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::SystemctlShowActiveState { .. })),
            "systemd state should not be queried after mountpoint probe error: {:?}",
            runner.requests()
        );
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
                exit_status: 32,
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

    // Intent: offline + mutable is the one Warn case -- the invariant is not yet
    //   held and the operator can re-seal.
    // Why it exists: this is the central detection signal for a mountpoint left
    //   writable while the pool is offline (the data-safety bug this guards).
    // Scenario: an out-of-band `chattr -i` left /mnt/storage writable offline.
    #[test]
    fn classify_mountpoint_immutability_offline_mutable_warns() {
        let finding = classify_mountpoint_immutability(
            "/mnt/storage",
            Some(false),
            ImmutabilityProbe::Mutable,
        );
        match finding {
            ImmutableFinding::Warn(msg) => {
                assert!(msg.contains("braid seal-mountpoint"), "hint missing: {msg}");
                // Under the boot-only model the hint must never tell the
                // operator to unlock to clear the warning.
                assert!(
                    !msg.contains("braid unlock"),
                    "must not suggest unlock: {msg}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    // Intent: online + immutable is the catastrophic Failure case -- a live pool
    //   root must never be sealed.
    // Why it exists: a sealed mounted root blocks all pool writes; the timing
    //   rule exists to prevent it, so doctor must flag it loudly if it happens.
    // Scenario: a bug or external interference sealed the mounted pool root.
    #[test]
    fn classify_mountpoint_immutability_online_immutable_fails() {
        let finding = classify_mountpoint_immutability(
            "/mnt/storage",
            Some(true),
            ImmutabilityProbe::Immutable,
        );
        assert!(matches!(finding, ImmutableFinding::Failure(_)));
    }

    // Intent: the two healthy steady states produce no finding.
    // Why it exists: sealed-offline and mounted-mutable are correct; doctor must
    //   stay quiet so the check is not noise.
    // Scenario: a normally sealed offline pool, and a normally mounted pool.
    #[test]
    fn classify_mountpoint_immutability_healthy_states_are_none() {
        assert_eq!(
            classify_mountpoint_immutability(
                "/mnt/storage",
                Some(false),
                ImmutabilityProbe::Immutable
            ),
            ImmutableFinding::None
        );
        assert_eq!(
            classify_mountpoint_immutability(
                "/mnt/storage",
                Some(true),
                ImmutabilityProbe::Mutable
            ),
            ImmutableFinding::None
        );
    }

    // Intent: an indeterminate immutability probe suppresses any finding,
    //   regardless of mount state.
    // Why it exists: an unsupported root fs / old kernel must not produce the
    //   misleading "not immutable; reseal" Warn -- the seal unit owns that
    //   signal (the bare-bool unwrap_or coin-flip this enum forecloses).
    // Scenario: `is_immutable` returned Err on an unsupported root filesystem.
    #[test]
    fn classify_mountpoint_immutability_indeterminate_probe_is_none() {
        for mounted in [Some(false), Some(true), None] {
            assert_eq!(
                classify_mountpoint_immutability(
                    "/mnt/storage",
                    mounted,
                    ImmutabilityProbe::Indeterminate
                ),
                ImmutableFinding::None,
                "mounted={mounted:?}"
            );
        }
    }

    // Intent: a failed mount probe (mount-state None) suppresses both severities.
    // Why it exists: a collapsed Some(false) would masquerade as "offline" and
    //   fire a false offline+mutable Warn when the pool is actually mounted but
    //   the probe failed (F3) -- the row a bare-bool `mounted` could not express.
    // Scenario: the mountpoint probe itself errored.
    #[test]
    fn classify_mountpoint_immutability_mount_probe_failure_is_none() {
        for probe in [
            ImmutabilityProbe::Immutable,
            ImmutabilityProbe::Mutable,
            ImmutabilityProbe::Indeterminate,
        ] {
            assert_eq!(
                classify_mountpoint_immutability("/mnt/storage", None, probe),
                ImmutableFinding::None,
                "probe={probe:?}"
            );
        }
    }
}
