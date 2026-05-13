use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{Config, mapper_name};
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::luks::{self, LuksError, OpenOutcome};
use crate::mapper_close::{CloseMapperError, close_mapper_with_retry};
use crate::membership::PoolMembership;
use crate::preview::{self, NoteLevel, PerDiskStyle, PreviewNote};
use crate::probe::{self, Filesystem, ProbeError};
use crate::progress::Sleeper;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{ByIdPath, ConfigDiskState, MapperName, MountPoint};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Luks(#[from] LuksError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    MountFailed(String),
    #[error("{0}")]
    DegradedRefused(String),
}

#[derive(Debug)]
pub struct UnlockAndMountFailure {
    pub error: MountError,
    pub opened_mappers: Vec<MapperName>,
}

/// Why a membership disk is missing from the pool at unlock time.
/// Used to format the structured `DegradedRefused` error in probe order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingReason {
    /// Device file does not exist on the host (`ConfigDiskState::Absent`).
    Unplugged,
    /// Device exists but `cryptsetup isLuks` exits non-zero — the LUKS
    /// magic is gone or otherwise unrecognizable. Distinct from Damaged
    /// because there is no metadata structure left to repair via
    /// `cryptsetup repair`.
    LuksHeaderUnreadable,
    /// Device exists, has valid LUKS magic, but `cryptsetup luksDump`
    /// fails to parse the metadata blocks. Potentially repairable via
    /// `cryptsetup repair --type luks2 <device>`.
    LuksHeaderDamaged,
}

impl MissingReason {
    fn is_luks_header_state(self) -> bool {
        matches!(
            self,
            MissingReason::LuksHeaderUnreadable | MissingReason::LuksHeaderDamaged
        )
    }
}

/// Format a structured `DegradedRefused` error message that names each
/// missing disk and the reason in probe order. Preserves the substrings
/// `"refusing to mount degraded"` and
/// `"braid <command_hint> --allow-degraded"` that existing tests anchor on.
///
/// `missing` is guaranteed non-empty by the caller.
fn format_degraded_refused(missing: &[(String, MissingReason)], command_hint: &str) -> String {
    let total = missing.len();
    let header = if total == 1 {
        "pool has 1 missing device -- refusing to mount degraded".to_owned()
    } else {
        format!("pool has {total} missing devices -- refusing to mount degraded")
    };

    let mut lines = vec![header];
    for (name, reason) in missing {
        let reason_text = match reason {
            MissingReason::Unplugged => "not found (unplugged?)",
            MissingReason::LuksHeaderUnreadable => "LUKS header unreadable",
            MissingReason::LuksHeaderDamaged => "LUKS header metadata damaged",
        };
        lines.push(format!("  {name}: {reason_text}"));
    }
    lines.push("new writes would have ZERO redundancy (single-profile chunks)".to_owned());
    lines.push(format!("hint: braid {command_hint} --allow-degraded"));
    if missing.iter().any(|(_, r)| r.is_luks_header_state()) {
        lines.push("run 'braid doctor' for recovery guidance".to_owned());
    }
    lines.join("\n")
}

/// Result of the read-only probe + validate phase.
#[derive(Debug)]
pub struct OpenPlan {
    /// Disks that need LUKS open (name, by_id pairs).
    pub to_unlock: Vec<(String, ByIdPath)>,
    /// At least one mapper was already open.
    pub any_open: bool,
    /// At least one membership disk was absent/damaged.
    pub any_missing_member: bool,
    /// Device path to use for mount (e.g. "/dev/mapper/braid-disk1").
    pub mount_device: String,
}

/// An observation produced during the read-only probe phase. Collected
/// by `plan_open_pool` and rendered by callers via `print_probe_events`
/// / `render_probe_events`. Kept separate from the plan so that error
/// returns still carry the per-disk context that preceded them (e.g.
/// `DegradedRefused`, UUID mismatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvent {
    AlreadyMounted { mount_point: String },
    DiskAbsent { name: String },
    DiskLuksHeaderUnreadable { name: String },
    DiskLuksHeaderDamaged { name: String },
    DiskAlreadyOpen { name: String },
    DiskAvailable { name: String },
}

impl ProbeEvent {
    /// Map a probe event to the shared `PreviewNote` shape used by the
    /// project-wide dry-run preview model. `AlreadyMounted` becomes an
    /// `Info` note; per-disk variants become `PerDisk` notes whose
    /// rendered bytes (under `PerDiskStyle::Bracketed`) match
    /// `render_probe_events`'s line format.
    pub fn to_preview_note(&self) -> PreviewNote {
        match self {
            ProbeEvent::AlreadyMounted { mount_point } => {
                PreviewNote::Info(format!("pool already mounted at {mount_point}"))
            }
            ProbeEvent::DiskAbsent { name } => PreviewNote::PerDisk {
                name: name.clone(),
                level: NoteLevel::Skip,
                message: "not found (unplugged?)".to_owned(),
            },
            ProbeEvent::DiskLuksHeaderUnreadable { name } => PreviewNote::PerDisk {
                name: name.clone(),
                level: NoteLevel::Skip,
                message: "LUKS header unreadable".to_owned(),
            },
            ProbeEvent::DiskLuksHeaderDamaged { name } => PreviewNote::PerDisk {
                name: name.clone(),
                level: NoteLevel::Skip,
                message: "LUKS header metadata damaged".to_owned(),
            },
            ProbeEvent::DiskAlreadyOpen { name } => PreviewNote::PerDisk {
                name: name.clone(),
                level: NoteLevel::Ok,
                message: "already open".to_owned(),
            },
            ProbeEvent::DiskAvailable { name } => PreviewNote::PerDisk {
                name: name.clone(),
                level: NoteLevel::Ok,
                message: "found".to_owned(),
            },
        }
    }
}

/// Outcome of `plan_open_pool`. `events` are always populated, even on
/// error, so callers can render them before propagating `result`.
pub struct PlanReport {
    pub events: Vec<ProbeEvent>,
    pub result: Result<Option<OpenPlan>, MountError>,
}

/// Probe membership disks, validate UUIDs, check degraded policy.
/// Returns the planning errors that `execute_mount_only` /
/// `execute_unlock_and_mount` would otherwise surface (degraded refusal,
/// UUID mismatch, no unlockable disks). No mutations -- safe for dry-run.
///
/// Events accumulate as probing proceeds and are returned on both
/// success and error paths. Callers typically render them (via
/// `print_probe_events`) before `?`-propagating `result`.
///
/// `result` is `Ok(None)` when the pool is already mounted.
pub fn plan_open_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    command_hint: &str,
) -> PlanReport {
    let mut events = Vec::new();
    let result = plan_open_pool_inner(
        runner,
        fs,
        config,
        membership,
        allow_degraded,
        command_hint,
        &mut events,
    );
    PlanReport { events, result }
}

fn plan_open_pool_inner<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    command_hint: &str,
    events: &mut Vec<ProbeEvent>,
) -> Result<Option<OpenPlan>, MountError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted -> None
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    if mp_result.exit_status == 0 {
        events.push(ProbeEvent::AlreadyMounted {
            mount_point: mount_point.to_string(),
        });
        return Ok(None);
    }

    // 2. Probe each membership disk
    let mut to_unlock = Vec::new();
    let mut any_open = false;
    let mut first_open_mapper: Option<String> = None;
    let mut missing: Vec<(String, MissingReason)> = Vec::new();

    let mut members: Vec<_> = membership.iter().collect();
    // Membership is UUID-keyed for persistence, but this probe emits
    // operator-visible rows. Keep the visible unlock order by disk name.
    members.sort_by(|(_, left), (_, right)| left.name.cmp(&right.name));

    for (expected_uuid, member) in members {
        let name = member.name.as_str();
        let probed = probe::probe_config_disk(runner, fs, &member.name, &member.by_id)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                events.push(ProbeEvent::DiskAbsent {
                    name: name.to_owned(),
                });
                missing.push((name.to_owned(), MissingReason::Unplugged));
            }
            ConfigDiskState::PresentNotLuks => {
                // Refine PresentNotLuks (luksUuid failed) into Unreadable vs
                // Damaged for diagnostic reporting only — do NOT propagate
                // this back into ConfigDiskState. add/replace must keep
                // seeing the coarse PresentNotLuks state to preserve their
                // destructive-format guards on potentially recoverable
                // damaged headers.
                let reason = match luks::probe_luks_header(runner, member.by_id.as_str()) {
                    luks::LuksHeaderState::Damaged => MissingReason::LuksHeaderDamaged,
                    // Unreadable, the inconsistent Ok-but-luksUuid-failed
                    // case, and ProbeFailed all collapse to Unreadable.
                    // Damaged is the only refinement we promote out of
                    // this branch because it has a distinct
                    // `cryptsetup repair` recovery story; everything else
                    // gets the conservative Unreadable label.
                    _ => MissingReason::LuksHeaderUnreadable,
                };
                events.push(match reason {
                    MissingReason::LuksHeaderDamaged => ProbeEvent::DiskLuksHeaderDamaged {
                        name: name.to_owned(),
                    },
                    _ => ProbeEvent::DiskLuksHeaderUnreadable {
                        name: name.to_owned(),
                    },
                });
                missing.push((name.to_owned(), reason));
            }
            ConfigDiskState::PresentLuks {
                uuid, mapper_open, ..
            } => {
                if expected_uuid != uuid {
                    return Err(MountError::Failed(format!(
                        "disk '{}' LUKS UUID mismatch at {}:\n  \
                             expected  {}\n  \
                             found     {}",
                        name, member.by_id, expected_uuid, uuid
                    )));
                }

                if *mapper_open {
                    events.push(ProbeEvent::DiskAlreadyOpen {
                        name: name.to_owned(),
                    });
                    any_open = true;
                    if first_open_mapper.is_none() {
                        first_open_mapper = Some(name.to_owned());
                    }
                } else {
                    events.push(ProbeEvent::DiskAvailable {
                        name: name.to_owned(),
                    });
                    to_unlock.push((name.to_owned(), member.by_id.clone()));
                }
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        return Err(MountError::Failed("no unlockable disks found".into()));
    }

    let any_missing_member = !missing.is_empty();

    // 4. Degraded check (before any mutations)
    if any_missing_member && !allow_degraded {
        return Err(MountError::DegradedRefused(format_degraded_refused(
            &missing,
            command_hint,
        )));
    }

    // 5. Compute mount device. Fallback must name a mapper that actually
    // exists -- using membership.disks.keys().next() would pick the
    // alphabetically-first disk even when it is Absent/PresentNotLuks,
    // producing a stale /dev/mapper/<first> path that mount would fail on.
    let mount_key = to_unlock
        .first()
        .map(|(k, _)| k.as_str())
        .or(first_open_mapper.as_deref())
        .unwrap_or("unknown");
    let mount_device = format!("/dev/mapper/{}", mapper_name(mount_key).0);

    Ok(Some(OpenPlan {
        to_unlock,
        any_open,
        any_missing_member,
        mount_device,
    }))
}

/// Render a probe-event sequence to a multi-line string. Pure: no I/O,
/// no global state. The exact byte output is a behavioral contract --
/// see `render_probe_events_formats_mixed_probe_result`.
///
/// Routes through the shared `preview::render_notes_for_stderr` helper
/// (Bracketed style) so that this function and the project-wide
/// `Preview` model stay byte-for-byte identical for these notes. The
/// per-event wording lives in `ProbeEvent::to_preview_note`.
pub fn render_probe_events(events: &[ProbeEvent]) -> String {
    let notes: Vec<PreviewNote> = events.iter().map(ProbeEvent::to_preview_note).collect();
    preview::render_notes_for_stderr(&notes, PerDiskStyle::Bracketed)
}

/// Thin stderr wrapper around `render_probe_events`. Callers invoke
/// this after `plan_open_pool` but before propagating any error, so
/// per-disk context always precedes a failure message.
pub fn print_probe_events(events: &[ProbeEvent]) {
    let notes: Vec<PreviewNote> = events.iter().map(ProbeEvent::to_preview_note).collect();
    let text = preview::render_notes_for_stderr_with(
        &notes,
        PerDiskStyle::Bracketed,
        color_enabled_for_stderr(),
    );
    if !text.is_empty() {
        eprint!("{text}");
    }
}

/// Compile output-only dry-run preview steps from a validated `OpenPlan`.
/// Real execution consumes the `OpenPlan` directly and constructs LUKS
/// requests through `luks::ensure_luks_open` / `ensure_luks_open_with_key_file`.
pub fn compile_open_steps(
    plan: &OpenPlan,
    mount_point: &MountPoint,
    key_file: Option<&Path>,
) -> Vec<Step> {
    let mut steps = Vec::new();

    for (name, by_id) in &plan.to_unlock {
        let mn = mapper_name(name);
        if let Some(kf) = key_file {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} -> {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: by_id.as_str().to_owned(),
                    mapper: mn.clone(),
                    key_file_path: kf.display().to_string(),
                }],
            });
        } else {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} -> {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: by_id.as_str().to_owned(),
                    mapper: mn.clone(),
                }],
            });
        }
    }

    steps.push(Step {
        risk: "safe",
        description: "btrfs device scan".into(),
        commands: vec![CmdRequest::BtrfsDeviceScanAll],
    });

    if plan.any_missing_member {
        steps.push(Step {
            risk: "safe",
            description: format!("mount -> {} (degraded)", mount_point),
            commands: vec![CmdRequest::MountWithOptions {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
                options: vec!["degraded".to_owned()],
            }],
        });
    } else {
        steps.push(Step {
            risk: "safe",
            description: format!("mount -> {}", mount_point),
            commands: vec![CmdRequest::Mount {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
            }],
        });
    }

    steps
}

/// Classify an unlock-time failure against the LUKS header state of the
/// affected disk, producing the best user-facing error.
///
/// The four match arms each represent a distinct user story:
///
/// - `Unreadable` → corruption is confirmed severe; emit the off-system
///   backup guidance regardless of what cryptsetup originally said.
/// - `Damaged` → corruption is confirmed at the metadata level; emit
///   the `cryptsetup repair` guidance with a safe-backup warning.
/// - `Ok` → the header is intact, so the failure really is about the
///   caller-supplied context (passphrase, invariant, device state,
///   generic I/O error, etc.); use `ok_fallback` unchanged.
/// - `ProbeFailed` → we genuinely do not know whether the header is
///   sound, so we must NOT confidently pick a narrative. Emit a
///   dedicated "diagnosis incomplete" message that surfaces both the
///   original cryptsetup signal (`original_summary`) and the probe
///   error, without narrowing the cause to any particular class of
///   failure. This is load-bearing: `explain_open_failure` is called
///   from all four unlock-failure sites, including the generic
///   non-auth open-loop path, so the wording cannot assume the
///   underlying cause was auth-related.
fn explain_open_failure(
    disk_name: &str,
    device: &str,
    header_state: luks::LuksHeaderState,
    original_summary: &str,
    ok_fallback: MountError,
) -> MountError {
    match header_state {
        luks::LuksHeaderState::Unreadable => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_unreadable_guidance()
        )),
        luks::LuksHeaderState::Damaged => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {}",
            luks::luks_header_damaged_guidance(device)
        )),
        luks::LuksHeaderState::Ok => ok_fallback,
        luks::LuksHeaderState::ProbeFailed(probe_err) => MountError::Failed(format!(
            "failed to unlock disk '{disk_name}' ({device}): {original_summary}. \
             LUKS header diagnosis could not be completed: {probe_err}. \
             Cannot determine whether this failure is due to LUKS header damage \
             or the reported cryptsetup error -- inspect the disk manually."
        )),
    }
}

fn credential_verify_targets(to_unlock: &[(String, ByIdPath)]) -> Vec<CredentialVerifyTarget> {
    to_unlock
        .iter()
        .map(|(name, by_id)| CredentialVerifyTarget {
            name: name.clone(),
            device: by_id.as_str().to_owned(),
        })
        .collect()
}

/// Mount-local noun for credential error messages that must match the
/// concrete unlock path the user selected.
fn credential_noun(c: Credential<'_>) -> &'static str {
    match c {
        Credential::Passphrase(_) => "passphrase",
        Credential::KeyFile(_) => "keyfile",
    }
}

/// Verify the selected credential against every to-unlock disk and then
/// open every to-unlock disk with the same credential. Keeps header-state
/// classification shared across passphrase and keyfile unlock paths.
fn open_disks_with_credential<R: CommandRunner>(
    runner: &R,
    to_unlock: &[(String, ByIdPath)],
    credential: Credential<'_>,
    color_enabled: bool,
    opened: &mut Vec<MapperName>,
) -> Result<(), MountError> {
    let noun = credential_noun(credential);
    let targets = credential_verify_targets(to_unlock);
    match verify_credential_for_targets(runner, &targets, credential, color_enabled, |line| {
        eprint!("{line}")
    }) {
        Ok(()) => {}
        Err(CredentialVerifyError::Rejected { target }) => {
            let original_summary = format!("{noun} rejected on '{}'", target.name);
            let ok_fallback =
                MountError::Failed(format!("wrong {noun} (rejected by {})", target.name));
            let header_state = luks::probe_luks_header(runner, &target.device);
            return Err(explain_open_failure(
                &target.name,
                &target.device,
                header_state,
                &original_summary,
                ok_fallback,
            ));
        }
        Err(CredentialVerifyError::Luks {
            target,
            source: e @ LuksError::OpenFailed { .. },
        }) => {
            let original_summary = format!("verify failed on '{}': {e}", target.name);
            let header_state = luks::probe_luks_header(runner, &target.device);
            return Err(explain_open_failure(
                &target.name,
                &target.device,
                header_state,
                &original_summary,
                MountError::Luks(e),
            ));
        }
        Err(CredentialVerifyError::Luks { source, .. }) => return Err(MountError::Luks(source)),
    }

    for (name, by_id) in to_unlock {
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {name}: unlocking..."),
            )
        );
        let outcome = match credential {
            Credential::Passphrase(pp) => luks::ensure_luks_open(runner, name, by_id, pp),
            Credential::KeyFile(kf) => {
                luks::ensure_luks_open_with_key_file(runner, name, by_id, kf)
            }
        };
        match outcome {
            Ok(OpenOutcome::Opened) => opened.push(mapper_name(name)),
            Ok(OpenOutcome::AlreadyOwned) => {}
            Err(e) => {
                let header_state = luks::probe_luks_header(runner, by_id.as_str());
                let (original_summary, ok_fallback) = match &e {
                    LuksError::OpenFailed {
                        exit_code: 2,
                        hint,
                        stderr,
                        ..
                    } => (
                        format!(
                            "cryptsetup open rejected on '{name}' after all planned-disk {noun} verification -- {hint} ({stderr})"
                        ),
                        MountError::Failed(format!(
                            "cryptsetup open rejected on '{name}' even though the {noun} was just \
                             verified against every planned disk. The credential likely changed between \
                             preflight and open (race or external LUKS manipulation)."
                        )),
                    ),
                    _ => {
                        let summary = format!("cryptsetup open failed on '{name}': {e}");
                        (summary, MountError::Luks(e))
                    }
                };
                return Err(explain_open_failure(
                    name,
                    by_id.as_str(),
                    header_state,
                    &original_summary,
                    ok_fallback,
                ));
            }
        }
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {name}: unlocked")
            )
        );
    }

    Ok(())
}

/// Execute a pre-built `OpenPlan` whose `to_unlock` is empty (all mappers
/// already open or no disks need unlocking).
///
/// Phases: reject non-empty `to_unlock` → btrfs device scan → mkdir + mount.
///
/// Non-empty `to_unlock` is a caller-contract violation and returns
/// `MountError::Failed("internal: execute_mount_only called with non-empty
/// plan.to_unlock")` in all builds. Callers that might hold a plan with
/// locked disks must dispatch to `execute_unlock_and_mount` instead.
///
/// Planning + probing + validation lives in `plan_open_pool`, which the
/// caller invokes first. This function does NOT plan; it only executes.
///
/// Returns `Ok(true)` once the mount succeeds. (Callers detect the
/// already-mounted case earlier, by `plan_open_pool` returning `None`.)
pub fn execute_mount_only<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    plan: &OpenPlan,
) -> Result<bool, MountError> {
    if !plan.to_unlock.is_empty() {
        return Err(MountError::Failed(
            "internal: execute_mount_only called with non-empty plan.to_unlock".into(),
        ));
    }
    let color_enabled = color_enabled_for_stderr();
    scan_and_mount(runner, fs, config, plan, color_enabled)
}

/// Execute a pre-built `OpenPlan` that has disks to unlock.
///
/// Phases: reject empty `to_unlock` → verify credential → open LUKS →
/// btrfs device scan → mkdir + mount.
///
/// Empty `to_unlock` is a caller-contract violation and returns
/// `MountError::Failed("internal: execute_unlock_and_mount called with empty
/// plan.to_unlock")` in all builds. Callers that might hold an empty plan
/// must dispatch to `execute_mount_only` instead.
///
/// Planning + probing + validation lives in `plan_open_pool`, which the
/// caller invokes first. This function does NOT plan; it only executes.
///
/// Returns `Ok(true)` once the mount succeeds.
pub fn execute_unlock_and_mount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    plan: &OpenPlan,
    credential: &crate::credential::OpenCredential,
) -> Result<bool, UnlockAndMountFailure> {
    let color_enabled = color_enabled_for_stderr();
    if plan.to_unlock.is_empty() {
        return Err(UnlockAndMountFailure {
            error: MountError::Failed(
                "internal: execute_unlock_and_mount called with empty plan.to_unlock".into(),
            ),
            opened_mappers: Vec::new(),
        });
    }

    let mut opened_mappers = Vec::new();
    let cred = credential.as_borrowed();
    open_disks_with_credential(
        runner,
        &plan.to_unlock,
        cred,
        color_enabled,
        &mut opened_mappers,
    )
    .map_err(|error| UnlockAndMountFailure {
        error,
        opened_mappers: opened_mappers.clone(),
    })?;

    scan_and_mount(runner, fs, config, plan, color_enabled).map_err(|error| UnlockAndMountFailure {
        error,
        opened_mappers,
    })
}

pub(crate) fn close_opened_mappers<R, S, F>(
    runner: &R,
    sleeper: &S,
    fs: &F,
    opened: &[MapperName],
    color_enabled: bool,
) -> Result<(), CloseMapperError>
where
    R: CommandRunner,
    S: Sleeper,
    F: Filesystem + ?Sized,
{
    if opened.is_empty() {
        return Ok(());
    }

    let forget_devs: Vec<String> = opened
        .iter()
        .map(|mapper| format!("/dev/mapper/{mapper}"))
        .filter(|path| fs.exists(path))
        .collect();
    if !forget_devs.is_empty() {
        let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget {
            devices: forget_devs,
        });
        match forget_result {
            Ok(r) if r.exit_status == 0 => {}
            Ok(r) => {
                emit_status(&status_line(
                    StatusTag::Warn,
                    color_enabled,
                    &format!(
                        "btrfs device scan --forget failed (exit {}): {} (continuing)",
                        r.exit_status,
                        r.stderr.trim()
                    ),
                ));
            }
            Err(e) => {
                emit_status(&status_line(
                    StatusTag::Warn,
                    color_enabled,
                    &format!("btrfs device scan --forget failed: {e} (continuing)"),
                ));
            }
        }
    }

    let mut first_error = None;
    for mapper in opened {
        let label = mapper
            .as_str()
            .strip_prefix("braid-")
            .unwrap_or(mapper.as_str());
        emit_status(&status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("disk {label}: locking..."),
        ));
        match close_mapper_with_retry(runner, sleeper, mapper, color_enabled) {
            Ok(()) => {
                emit_status(&status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {label}: locked"),
                ));
            }
            Err(e) => {
                emit_status(&status_line(
                    StatusTag::Fail,
                    color_enabled,
                    &format!("disk {label}: {e}"),
                ));
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    match first_error {
        None => {
            emit_status("cleanup: closed LUKS mappers opened by this command.\n");
            Ok(())
        }
        Some(e) => {
            emit_status(&format!(
                "cleanup failed: one or more LUKS mappers opened by this command could not be \
                 closed; run 'braid lock' after resolving the issue. First cleanup error: {e}\n"
            ));
            Err(e)
        }
    }
}

/// Shared tail for both execute entry points: btrfs device scan, ensure the
/// mount point exists, then mount (with `degraded` when any membership disk
/// is missing).
fn scan_and_mount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    _fs: &F,
    config: &Config,
    plan: &OpenPlan,
    color_enabled: bool,
) -> Result<bool, MountError> {
    let mount_point = config.mount_point();

    eprint!(
        "{}",
        status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("pool: mounting {mount_point}..."),
        )
    );

    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    if scan.exit_status != 0 {
        return Err(MountError::Failed(format!(
            "btrfs device scan failed (exit {}): {}",
            scan.exit_status,
            scan.stderr.trim()
        )));
    }

    let _ = std::fs::create_dir_all(mount_point.as_str());

    let mount_result = if plan.any_missing_member {
        runner.run(&CmdRequest::MountWithOptions {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        runner.run(&CmdRequest::Mount {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
        })?
    };

    if mount_result.exit_status != 0 {
        return Err(MountError::MountFailed(format!(
            "mount failed (exit {}): {}",
            mount_result.exit_status,
            mount_result.stderr.trim()
        )));
    }

    eprint!(
        "{}",
        status_line(
            StatusTag::Ok,
            color_enabled,
            &format!("pool: mounted {mount_point}")
        )
    );

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::credential::OpenCredential;
    use crate::secret::Passphrase;
    use crate::test_fixtures::{
        MOUNT_TEST_PASSPHRASE_BYTES, NoopSleeper, arbitrary_fallback, base_two_disk_runner,
        direct_two_disk_fs_with_mappers, direct_two_disk_open_runner, direct_two_disk_plan,
        disk_member, err_raw, is_luks_fail, is_luks_ok, luks_dump_text_fail, luks_dump_text_ok,
        luks_uuid_ok, mount_fs, ok_raw, open_and_mount_for_test, test_config, test_passphrase,
        test_passphrase_fail, three_disk_membership, two_disk_membership,
    };
    use crate::types::{ByIdPath, LuksUuid, MountPoint};
    use zeroize::Zeroizing;

    /// Intent: `execute_unlock_and_mount` must reject an empty `to_unlock`
    /// plan with a typed internal error BEFORE running any LUKS or mount
    /// commands.
    ///
    /// Why: The split of the legacy combined entry point into two
    /// points encodes credential-presence via the signature, but a caller
    /// could still route a plan whose `to_unlock` is empty into the
    /// unlock-and-mount path. That is a caller-contract violation. The
    /// residual check here replaces half of the original bidirectional
    /// runtime validation; without it, a bad caller wiring would silently
    /// attempt to mount via the unlock path and possibly hit downstream
    /// commands in a confusing order. If this check regresses to
    /// `debug_assert!` or is deleted, this test fails in release builds.
    ///
    /// Scenario: construct an `OpenPlan` directly with empty `to_unlock`,
    /// pass it to `execute_unlock_and_mount` with any credential, and
    /// confirm the typed `MountError::Failed(msg)` fires with a message
    /// naming the function and the violated precondition.
    #[test]
    fn execute_unlock_and_mount_rejects_empty_plan() {
        let config = test_config();
        let fs = mount_fs(&[]);
        // Runner with no outputs wired — if the guard lets us through,
        // the first real command (btrfs device scan or cryptsetup verify)
        // will panic on lookup, which would also fail the test.
        let runner = MockRunner::default();

        let plan = OpenPlan {
            to_unlock: Vec::new(),
            any_open: true,
            any_missing_member: false,
            mount_device: "/dev/mapper/braid-disk1".to_owned(),
        };

        let cred = test_passphrase();
        let res = execute_unlock_and_mount(&runner, &fs, &config, &plan, &cred);
        match res {
            Err(UnlockAndMountFailure {
                error: MountError::Failed(msg),
                opened_mappers,
            }) => {
                assert!(
                    opened_mappers.is_empty(),
                    "internal precondition failure must not report opened mappers"
                );
                assert!(
                    msg.contains("execute_unlock_and_mount called with empty plan.to_unlock"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected MountError::Failed, got {other:?}"),
        }
    }

    /// Intent: `execute_mount_only` must reject a non-empty `to_unlock`
    /// plan with a typed internal error BEFORE running any mount commands.
    ///
    /// Why: Symmetric counterpart to
    /// `execute_unlock_and_mount_rejects_empty_plan`. The type system
    /// cannot express "plan.to_unlock is empty" via `&OpenPlan`, so this
    /// runtime check is what catches a caller that routes a plan with
    /// locked disks into the mount-only path. Without it, `scan_and_mount`
    /// would attempt to mount the btrfs device while some LUKS members are
    /// still locked, producing an obscure mount-layer error far from the
    /// root cause. If this check regresses or is deleted, this test fails.
    ///
    /// Scenario: construct an `OpenPlan` directly with one disk still to
    /// unlock, pass it to `execute_mount_only`, and confirm the typed
    /// `MountError::Failed(msg)` fires.
    #[test]
    fn execute_mount_only_rejects_non_empty_plan() {
        let config = test_config();
        let fs = mount_fs(&[]);
        let runner = MockRunner::default();

        let plan = OpenPlan {
            to_unlock: vec![(
                "disk1".to_owned(),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
            )],
            any_open: false,
            any_missing_member: false,
            mount_device: "/dev/mapper/braid-disk1".to_owned(),
        };

        let res = execute_mount_only(&runner, &fs, &config, &plan);
        match res {
            Err(MountError::Failed(msg)) => {
                assert!(
                    msg.contains("execute_mount_only called with non-empty plan.to_unlock"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected MountError::Failed, got {other:?}"),
        }
    }

    /// Intent: When the pool is already mounted, open_and_mount_pool should
    /// return Ok(false) without issuing any LUKS commands.
    ///
    /// Why: Callers use the return value to decide post-mount actions
    /// (e.g. unlock refreshes metadata, recover continues to rebuild).
    ///
    /// Scenario: Pool was previously unlocked and is still mounted. A
    /// redundant mount attempt should be a no-op.
    #[test]
    fn mount_already_mounted_returns_false() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[]);

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint"),
        );

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, false, "unlock");

        assert!(!result.unwrap());
    }

    /// Intent: Two healthy disks with LUKS closed should be opened, scanned,
    /// and mounted successfully.
    ///
    /// Why: This is the core happy path that both unlock and recover rely on.
    ///
    /// Scenario: 2-disk RAID1, both present, both LUKS-closed. Passphrase
    /// provided via file. All commands succeed.
    #[test]
    fn mount_two_disk_happy_path() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let runner = base_two_disk_runner()
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        assert!(result.unwrap());
    }

    /// Intent: `plan_open_pool` renders probe rows and unlock targets in
    /// disk-name order even though membership is keyed by LUKS UUID.
    ///
    /// Why: Raw UUID-key iteration is effectively random to operators and
    /// made keyfile/passphrase verification rows shuffle between pools.
    ///
    /// Scenario: membership is deliberately inserted so UUID order is the
    /// reverse of disk-name order; planning must still report and unlock
    /// `a-disk`, then `m-disk`, then `z-disk`.
    #[test]
    fn plan_open_pool_sorts_operator_visible_members_by_disk_name() {
        let config = test_config();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-a",
            "/dev/disk/by-id/virtio-m",
            "/dev/disk/by-id/virtio-z",
        ]);

        let mut membership = PoolMembership::empty();
        for (seed, name, by_id) in [
            (99, "a-disk", "/dev/disk/by-id/virtio-a"),
            (50, "m-disk", "/dev/disk/by-id/virtio-m"),
            (1, "z-disk", "/dev/disk/by-id/virtio-z"),
        ] {
            let (uuid, member) = disk_member(seed, name, by_id);
            membership.insert(uuid, member).expect("insert member");
        }

        let (uuid_a_req, uuid_a_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-a",
            "00000000-0000-0000-0000-000000000063",
        );
        let (uuid_m_req, uuid_m_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-m",
            "00000000-0000-0000-0000-000000000032",
        );
        let (uuid_z_req, uuid_z_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-z",
            "00000000-0000-0000-0000-000000000001",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid_a_req, uuid_a_out)
            .with_output(uuid_m_req, uuid_m_out)
            .with_output(uuid_z_req, uuid_z_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-a")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-m")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-z")
            .with_mappers_closed(&["braid-a-disk", "braid-m-disk", "braid-z-disk"]);

        let report = plan_open_pool(&runner, &fs, &config, &membership, false, "unlock");
        let plan = report
            .result
            .expect("planning should succeed")
            .expect("pool should need unlock");

        assert_eq!(
            report.events,
            vec![
                ProbeEvent::DiskAvailable {
                    name: "a-disk".to_owned()
                },
                ProbeEvent::DiskAvailable {
                    name: "m-disk".to_owned()
                },
                ProbeEvent::DiskAvailable {
                    name: "z-disk".to_owned()
                },
            ]
        );

        let to_unlock_names: Vec<&str> = plan
            .to_unlock
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(to_unlock_names, ["a-disk", "m-disk", "z-disk"]);
        assert_eq!(plan.mount_device, "/dev/mapper/braid-a-disk");
    }

    /// Intent: When a disk is absent and --allow-degraded is passed, the pool
    /// should mount with the degraded option.
    ///
    /// Why: Recovery after interrupted remove may leave a disk absent. The
    /// pool must still be mountable.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent. allow_degraded=true. Mount uses
    /// MountWithOptions with "degraded".
    #[test]
    fn mount_degraded_with_flag() {
        let config = test_config();
        let membership = three_disk_membership();
        // disk3 is absent — not in fs paths
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o degraded"),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            true,
            "unlock",
        );

        assert!(result.unwrap());
    }

    /// Intent: When a disk is absent and --allow-degraded is NOT passed, the
    /// mount must be refused with a clear error including the command hint.
    ///
    /// Why: Principle 1 requires explicit opt-in for degraded mounts.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent, allow_degraded=false. The error
    /// must mention "braid recover --allow-degraded" when command_hint is "recover".
    #[test]
    fn mount_degraded_refused() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"));
        // No mount mock — should never reach mount

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "recover",
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            matches!(&err, MountError::DegradedRefused(_)),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );
    }

    /// Intent: format_degraded_refused must surface the disk name, the
    /// reason ("LUKS header unreadable"), the substring contracts that
    /// existing tests anchor on, and the singular "1 missing device" form
    /// for a single-disk failure.
    ///
    /// Why: The Test 7 VM scenario hits exactly this shape (one raw
    /// member in a 2-disk pool). If any of these substrings drift, both
    /// the existing degraded-refused unit tests and the VM test would
    /// fail with confusing diffs.
    ///
    /// Scenario: A pool has one missing member ("raw") with an unreadable
    /// LUKS header, command_hint is "unlock".
    #[test]
    fn format_degraded_refused_single_unreadable_includes_disk_name_and_reason() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            msg.contains("refusing to mount degraded"),
            "missing 'refusing to mount degraded': {msg}"
        );
        assert!(
            msg.contains("braid unlock --allow-degraded"),
            "missing 'braid unlock --allow-degraded': {msg}"
        );
        assert!(
            msg.contains("raw: LUKS header unreadable"),
            "missing per-disk line 'raw: LUKS header unreadable': {msg}"
        );
        assert!(
            msg.contains("1 missing device"),
            "expected singular '1 missing device': {msg}"
        );
        // Make sure the singular form is not "1 missing devices"
        assert!(
            !msg.contains("1 missing devices"),
            "singular form should not have trailing 's': {msg}"
        );
        assert!(
            msg.contains("new writes would have ZERO redundancy"),
            "missing redundancy warning preserved from old message: {msg}"
        );
    }

    /// Intent: format_degraded_refused must enumerate disks in probe
    /// order, even when reasons are interleaved (unplugged → unreadable
    /// → unplugged).
    ///
    /// Why: This is the regression test for the parallel-vector design
    /// considered in an earlier draft. Two parallel `Vec<String>` lists
    /// would group all unplugged disks before all unreadable ones,
    /// reordering the final error relative to the preceding eprintln!
    /// status stream. The test asserts byte-offset ordering to make any
    /// future regression to a category-grouped layout fail loudly.
    ///
    /// Scenario: A 3-missing-disk pool: disk2 unplugged, disk3
    /// unreadable, disk5 unplugged. command_hint is "recover".
    #[test]
    fn format_degraded_refused_mixed_reasons_enumerates_each_disk_in_order() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
                ("disk5".to_owned(), MissingReason::Unplugged),
            ],
            "recover",
        );

        assert!(
            msg.contains("disk2: not found (unplugged?)"),
            "missing disk2 line: {msg}"
        );
        assert!(
            msg.contains("disk3: LUKS header unreadable"),
            "missing disk3 line: {msg}"
        );
        assert!(
            msg.contains("disk5: not found (unplugged?)"),
            "missing disk5 line: {msg}"
        );

        // Probe-order assertion: disk2 < disk3 < disk5 in byte offsets.
        let pos_disk2 = msg.find("disk2:").expect("disk2 should appear");
        let pos_disk3 = msg.find("disk3:").expect("disk3 should appear");
        let pos_disk5 = msg.find("disk5:").expect("disk5 should appear");
        assert!(
            pos_disk2 < pos_disk3,
            "disk2 must appear before disk3 (probe order): {msg}"
        );
        assert!(
            pos_disk3 < pos_disk5,
            "disk3 must appear before disk5 (probe order): {msg}"
        );

        assert!(
            msg.contains("3 missing devices"),
            "expected plural '3 missing devices': {msg}"
        );
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "missing 'braid recover --allow-degraded': {msg}"
        );
    }

    /// Intent: format_degraded_refused must use "1 missing device" for a
    /// single missing disk and "N missing devices" for two or more.
    ///
    /// Why: User-facing pluralization is small but cumulative — trivial
    /// to get right with a one-line check, distracting if wrong.
    ///
    /// Scenario: One call with one entry, one call with two entries;
    /// assert the singular and plural forms are correct.
    #[test]
    fn format_degraded_refused_uses_singular_for_one_disk_and_plural_otherwise() {
        let one = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            one.contains("1 missing device") && !one.contains("1 missing devices"),
            "expected singular '1 missing device' (no trailing s): {one}"
        );

        let two = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
            ],
            "unlock",
        );
        assert!(
            two.contains("2 missing devices"),
            "expected plural '2 missing devices': {two}"
        );
    }

    /// Intent: format_degraded_refused must never reference local LUKS
    /// header backup paths.
    ///
    /// Why: The cross-command negative invariant established in
    /// `plans/wip/cheeky-questing-popcorn.md` says that user-facing
    /// recovery messages must use generic off-system backup language and
    /// must not point at `/var/lib/braid/luks-headers/` or `.luksheader`
    /// files. Locking this in at the formatter level means the invariant
    /// holds for every caller of `DegradedRefused`, present and future.
    ///
    /// Scenario: A non-empty input with both reason kinds. Both negative
    /// substrings must be absent.
    #[test]
    fn format_degraded_refused_does_not_reference_local_header_backups() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderUnreadable),
            ],
            "unlock",
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /// Intent: format_degraded_refused must surface a distinct
    /// "LUKS header metadata damaged" line for `MissingReason::LuksHeaderDamaged`.
    ///
    /// Why: damaged metadata has a different recovery story
    /// (`cryptsetup repair`) than an unreadable header. Collapsing both
    /// into the same label would hide the actionable distinction the
    /// upstream probe just made.
    ///
    /// Scenario: a single declared disk whose LUKS magic is intact but
    /// luksDump fails to parse the metadata blocks.
    #[test]
    fn format_degraded_refused_damaged_includes_disk_name_and_reason() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderDamaged)],
            "unlock",
        );
        assert!(
            msg.contains("raw: LUKS header metadata damaged"),
            "missing damaged label: {msg}"
        );
        assert!(
            msg.contains("1 missing device") && !msg.contains("1 missing devices"),
            "expected singular: {msg}"
        );
    }

    /// Intent: format_degraded_refused must append a doctor-guidance footer
    /// when at least one disk has an Unreadable LUKS header.
    ///
    /// Why: the inline list keeps short labels; the proactive doctor
    /// command holds the full recovery guidance. The footer is the bridge
    /// between the two.
    ///
    /// Scenario: a single LuksHeaderUnreadable disk; the footer is the
    /// last line of the message.
    #[test]
    fn format_degraded_refused_unreadable_includes_doctor_footer() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderUnreadable)],
            "unlock",
        );
        assert!(
            msg.contains("run 'braid doctor' for recovery guidance"),
            "missing doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must append a doctor-guidance footer
    /// when at least one disk has a Damaged LUKS header.
    ///
    /// Why: same bridge as the unreadable case — the per-disk label is
    /// short, and `braid doctor` carries the full repair guidance.
    ///
    /// Scenario: a single LuksHeaderDamaged disk.
    #[test]
    fn format_degraded_refused_damaged_includes_doctor_footer() {
        let msg = format_degraded_refused(
            &[("raw".to_owned(), MissingReason::LuksHeaderDamaged)],
            "unlock",
        );
        assert!(
            msg.contains("run 'braid doctor' for recovery guidance"),
            "missing doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must NOT append the doctor footer
    /// for an Unplugged-only failure.
    ///
    /// Why: a hot-unplugged cable does not need recovery guidance — the
    /// fix is to plug the cable back in. Adding doctor noise to that case
    /// would dilute the signal where it actually matters.
    ///
    /// Scenario: a single Unplugged disk.
    #[test]
    fn format_degraded_refused_unplugged_only_omits_doctor_footer() {
        let msg =
            format_degraded_refused(&[("raw".to_owned(), MissingReason::Unplugged)], "unlock");
        assert!(
            !msg.contains("braid doctor"),
            "unplugged-only must not include doctor footer: {msg}"
        );
    }

    /// Intent: format_degraded_refused must include the doctor footer at
    /// most once, even when multiple LUKS-header-state disks are present
    /// alongside an unplugged disk.
    ///
    /// Why: emitting the footer per-disk would be noisy and could
    /// confuse the user about whether each line is a separate
    /// recommendation. The footer is a single trailing instruction.
    ///
    /// Scenario: one Unplugged + one Damaged disk; both labels appear,
    /// the footer appears exactly once.
    #[test]
    fn format_degraded_refused_mixed_includes_doctor_footer_once() {
        let msg = format_degraded_refused(
            &[
                ("disk2".to_owned(), MissingReason::Unplugged),
                ("disk3".to_owned(), MissingReason::LuksHeaderDamaged),
            ],
            "unlock",
        );
        assert!(
            msg.contains("disk2: not found (unplugged?)"),
            "missing unplugged line: {msg}"
        );
        assert!(
            msg.contains("disk3: LUKS header metadata damaged"),
            "missing damaged line: {msg}"
        );
        assert_eq!(
            msg.matches("run 'braid doctor' for recovery guidance")
                .count(),
            1,
            "doctor footer must appear exactly once: {msg}"
        );
    }

    /// Intent: When a passphrase is verified against disk1 but rejected by
    /// disk2, the error must name both disks.
    ///
    /// Why: The single-passphrase invariant (Principle 4) may be violated by
    /// external LUKS manipulation. The error must help the user identify which
    /// disk is different.
    ///
    /// Scenario: 2-disk RAID1, passphrase verified on disk1, disk2 rejects it.
    #[test]
    fn mount_passphrase_mismatch_names_disk() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk2");
        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, MOUNT_TEST_PASSPHRASE_BYTES.to_vec(), tp_out)
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                ok_raw("cryptsetup isLuks"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                err_raw(
                    "cryptsetup open",
                    2,
                    "No key available with this passphrase.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 rejects passphrase");
        let msg = err.to_string();
        assert!(
            msg.contains("disk2"),
            "error should name the failing disk, got: {msg}"
        );
        assert!(
            !msg.contains("disk1"),
            "preflight rejection should not report disk1 as the verification disk: {msg}"
        );
    }

    /// Intent: When all disks are absent and none are already open, the helper
    /// must return a clear error.
    ///
    /// Why: Cannot mount what doesn't exist.
    ///
    /// Scenario: 2-disk pool, both disks unplugged.
    #[test]
    fn mount_no_unlockable_disks() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[]); // no devices present

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        );

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, false, "unlock");

        let err = result.expect_err("should fail with no unlockable disks");
        let msg = err.to_string();
        assert!(
            msg.contains("no unlockable disks"),
            "expected 'no unlockable disks', got: {msg}"
        );
    }

    /// Intent: When all LUKS mappers are already open, the helper should skip
    /// passphrase prompting and proceed directly to scan + mount.
    ///
    /// Why: Idempotency. User may have partially recovered manually before
    /// running braid recover.
    ///
    /// Scenario: 2-disk pool, both mappers already open, pool not yet mounted.
    #[test]
    fn mount_skip_already_open() {
        let config = test_config();
        let membership = two_disk_membership();
        // Devices exist and mappers exist
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "11111111-1111-1111-1111-111111111111",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mapper_open(
                "braid-disk1",
                "/dev/vda",
                "11111111-1111-1111-1111-111111111111",
            )
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            // No passphrase or LUKS open mocks — should not be called
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, false, "unlock");

        assert!(result.unwrap());
    }

    /// Intent: When the alphabetically-first membership disk is absent and
    /// all surviving members already have their mappers open, `plan_open_pool`
    /// must set `mount_device` to an open mapper -- never to the absent
    /// first disk's mapper path.
    ///
    /// Why: This is the primary regression test for the
    /// `membership.disks.keys().next()` fallback bug. With `to_unlock` empty
    /// and `any_open == true`, the old code picked the BTreeMap's first key
    /// (e.g. "disk1") with no state filter, producing
    /// `/dev/mapper/braid-disk1` even when disk1 was `Absent`. The mount
    /// then failed with a confusing "no such device" instead of mounting
    /// degraded via a mapper that actually existed. If this fix is reverted
    /// -- changing `or(first_open_mapper.as_deref())` back to
    /// `or_else(|| membership.disks.keys().next()...)` -- this test fails.
    ///
    /// Scenario: 3-disk pool, disk1 unplugged, disk2 and disk3 present with
    /// mappers already open (e.g. a second unlock attempt after the first
    /// opened everything but never reached mount). --allow-degraded set.
    #[test]
    fn plan_open_pool_degraded_first_absent_picks_open_mapper() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = mount_fs(&[
            // disk1 absent -- not in fs paths
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-disk3",
        ]);

        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk3")
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            .with_mapper_open(
                "braid-disk3",
                "/dev/vdc",
                "33333333-3333-3333-3333-333333333333",
            );

        let report = plan_open_pool(&runner, &fs, &config, &membership, true, "unlock");
        let plan = report
            .result
            .expect("plan should succeed with --allow-degraded")
            .expect("pool is not mounted -- plan should not be None");

        assert!(
            plan.to_unlock.is_empty(),
            "to_unlock must be empty (all surviving mappers already open): {:?}",
            plan.to_unlock
        );
        assert!(plan.any_open, "any_open must be true");
        assert!(
            plan.any_missing_member,
            "any_missing_member must be true (disk1 absent)"
        );
        assert_eq!(
            plan.mount_device, "/dev/mapper/braid-disk2",
            "mount_device must name the first open mapper, not the absent first disk"
        );
        assert_eq!(
            report.events,
            vec![
                ProbeEvent::DiskAbsent {
                    name: "disk1".to_owned()
                },
                ProbeEvent::DiskAlreadyOpen {
                    name: "disk2".to_owned()
                },
                ProbeEvent::DiskAlreadyOpen {
                    name: "disk3".to_owned()
                },
            ],
            "event sequence must match probe order"
        );
    }

    /// Intent: `render_probe_events` must produce byte-for-byte stable
    /// output for every `ProbeEvent` variant.
    ///
    /// Why: The pre-refactor behavior of `plan_open_pool` was to emit
    /// these exact lines inline via `eprintln!`. The function now
    /// routes through the shared `preview::render_notes_for_stderr`
    /// helper, so wording/padding/tag drift can come from either side
    /// (probe-event -> note mapping or the shared renderer). A failure
    /// here means user-visible stderr output from unlock/recover has
    /// shifted.
    ///
    /// Scenario: a single fixture vector containing every variant,
    /// including both disk states and the already-mounted header line.
    #[test]
    fn render_probe_events_formats_mixed_probe_result() {
        let events = vec![
            ProbeEvent::AlreadyMounted {
                mount_point: "/mnt/storage".to_owned(),
            },
            ProbeEvent::DiskAbsent {
                name: "disk1".to_owned(),
            },
            ProbeEvent::DiskLuksHeaderUnreadable {
                name: "disk2".to_owned(),
            },
            ProbeEvent::DiskLuksHeaderDamaged {
                name: "disk3".to_owned(),
            },
            ProbeEvent::DiskAlreadyOpen {
                name: "disk4".to_owned(),
            },
            ProbeEvent::DiskAvailable {
                name: "disk5".to_owned(),
            },
        ];

        let rendered = render_probe_events(&events);

        let expected = "\
pool already mounted at /mnt/storage
[skip] disk disk1: not found (unplugged?)
[skip] disk disk2: LUKS header unreadable
[skip] disk disk3: LUKS header metadata damaged
[ok]   disk disk4: already open
[ok]   disk disk5: found
";

        assert_eq!(
            rendered, expected,
            "render_probe_events output drifted from the pre-refactor stderr format"
        );
    }

    /*
     * Intent: each ProbeEvent variant must round-trip through
     * `to_preview_note` + `preview::render_notes_for_stderr(Bracketed)`
     * to the legacy `render_probe_events` line for that variant -- per
     * variant, byte-for-byte.
     *
     * Why it exists: PR 0 introduces a per-variant adapter
     * (`ProbeEvent::to_preview_note`) and routes `render_probe_events`
     * through the shared preview renderer. The aggregated mixed-events
     * test (`render_probe_events_formats_mixed_probe_result`) catches
     * drift in the combined output, but a per-variant test localises
     * any wording drift to a single arm of `to_preview_note`. Listed in
     * the plan's risks section.
     *
     * Scenario: render each ProbeEvent variant individually, both via
     * the legacy public API and via the new note adapter, and assert
     * byte equality with the pinned legacy line for that variant.
     */
    #[test]
    fn probe_event_to_preview_note_preserves_byte_format() {
        let cases: Vec<(ProbeEvent, &'static str)> = vec![
            (
                ProbeEvent::AlreadyMounted {
                    mount_point: "/mnt/storage".to_owned(),
                },
                "pool already mounted at /mnt/storage\n",
            ),
            (
                ProbeEvent::DiskAbsent {
                    name: "disk1".to_owned(),
                },
                "[skip] disk disk1: not found (unplugged?)\n",
            ),
            (
                ProbeEvent::DiskLuksHeaderUnreadable {
                    name: "disk2".to_owned(),
                },
                "[skip] disk disk2: LUKS header unreadable\n",
            ),
            (
                ProbeEvent::DiskLuksHeaderDamaged {
                    name: "disk3".to_owned(),
                },
                "[skip] disk disk3: LUKS header metadata damaged\n",
            ),
            (
                ProbeEvent::DiskAlreadyOpen {
                    name: "disk4".to_owned(),
                },
                "[ok]   disk disk4: already open\n",
            ),
            (
                ProbeEvent::DiskAvailable {
                    name: "disk5".to_owned(),
                },
                "[ok]   disk disk5: found\n",
            ),
        ];

        for (event, expected) in &cases {
            let via_legacy = render_probe_events(std::slice::from_ref(event));
            assert_eq!(
                &via_legacy, *expected,
                "render_probe_events drifted for {event:?}",
            );

            let note = event.to_preview_note();
            let via_adapter = preview::render_notes_for_stderr(
                std::slice::from_ref(&note),
                PerDiskStyle::Bracketed,
            );
            assert_eq!(
                &via_adapter, *expected,
                "to_preview_note + render_notes_for_stderr drifted for {event:?}",
            );
        }
    }

    /// Intent: `plan_open_pool` must return the events it accumulated
    /// even when it returns an error. Users and tests rely on seeing
    /// the per-disk probe context *before* a refusal error.
    ///
    /// Why: Before this refactor, `plan_open_pool` wrote probe lines to
    /// stderr inline, so those lines appeared ahead of any subsequent
    /// error (degraded-refused, UUID mismatch, no unlockable disks). A
    /// naive `Result<Option<OpenPlan>, MountError>` shape would drop
    /// those events on the `Err` path. This test pins the
    /// events-always contract so that regression is caught.
    ///
    /// Scenario: 3-disk pool with disk3 absent and
    /// `allow_degraded=false`. `plan_open_pool` must return
    /// `DegradedRefused` AND a `report.events` vector containing the
    /// two Available disks plus the one Absent disk.
    #[test]
    fn plan_open_pool_emits_events_before_degraded_refused() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            // disk3 absent -- not in fs paths
        ]);

        let runner = base_two_disk_runner();

        let report = plan_open_pool(&runner, &fs, &config, &membership, false, "unlock");

        let err = report
            .result
            .expect_err("should refuse degraded mount without --allow-degraded");
        assert!(
            matches!(&err, MountError::DegradedRefused(_)),
            "expected DegradedRefused, got: {err:?}"
        );
        assert_eq!(
            report.events,
            vec![
                ProbeEvent::DiskAvailable {
                    name: "disk1".to_owned()
                },
                ProbeEvent::DiskAvailable {
                    name: "disk2".to_owned()
                },
                ProbeEvent::DiskAbsent {
                    name: "disk3".to_owned()
                },
            ],
            "events accumulated during probe must survive the Err return"
        );
    }

    /// Intent: End-to-end, the degraded-mount path with an absent first disk
    /// and all surviving mappers open must issue the MountWithOptions call
    /// against the open mapper, not the stale first-disk mapper.
    ///
    /// Why: Proves that `plan.mount_device` flows through unchanged into the
    /// actual `CmdRequest::MountWithOptions` -- catching any future refactor
    /// that would recompute a mount device downstream. The MockRunner is
    /// strict about seeded requests: if the code attempted to mount
    /// `/dev/mapper/braid-disk1`, it would surface as a missing-mock error
    /// and fail the test.
    ///
    /// Scenario: same setup as the plan-level regression above; additionally
    /// seed `BtrfsDeviceScanAll` and the expected degraded-mount command.
    #[test]
    fn mount_degraded_first_absent_all_open_uses_open_mapper() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-disk3",
        ]);

        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let (uuid3_req, uuid3_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk3",
            "33333333-3333-3333-3333-333333333333",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid2_req, uuid2_out)
            .with_output(uuid3_req, uuid3_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk3")
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            .with_mapper_open(
                "braid-disk3",
                "/dev/vdc",
                "33333333-3333-3333-3333-333333333333",
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk2".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o degraded"),
            );

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, true, "unlock");

        assert!(
            result.unwrap(),
            "degraded mount must succeed using the first open mapper"
        );
    }

    /// Intent: When a disk's probed LUKS UUID doesn't match pool.json's stored
    /// UUID, unlock must fatally error before attempting to open the device.
    ///
    /// Why: A UUID mismatch means the physical drive has been swapped,
    /// reformatted, or corrupted. Proceeding would mount the wrong data.
    ///
    /// Scenario: 2-disk RAID1. disk1 has a stored luks_uuid from a prior
    /// unlock, but the device now reports a different UUID (drive was swapped).
    /// Both LUKS devices are closed.
    #[test]
    fn mount_luks_uuid_mismatch_closed() {
        let config = test_config();
        let mut membership = two_disk_membership();
        let disk1_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let disk1 = membership
            .remove_by_uuid(&disk1_uuid)
            .expect("disk1 fixture member");
        membership
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                disk1,
            )
            .expect("replace disk1 fixture UUID");

        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        // Override base's disk1 UUID seed with a value that mismatches the
        // stored luks_uuid (HashMap insert semantics on `with_output`).
        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        );
        let runner = base_two_disk_runner().with_output(uuid1_req, uuid1_out);

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, false, "unlock");

        let err = result.expect_err("should fail on LUKS UUID mismatch");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("111111"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }

    /// Intent: UUID mismatch must be caught even when the LUKS mapper is
    /// already open (e.g. from a previous partial unlock or manual intervention).
    ///
    /// Why: The check must fire in both PresentLuks branches — mapper_open
    /// status doesn't make a swapped drive safe.
    ///
    /// Scenario: Same as mount_luks_uuid_mismatch_closed, but disk1's mapper
    /// is already open.
    #[test]
    fn mount_luks_uuid_mismatch_already_open() {
        let config = test_config();
        let mut membership = two_disk_membership();
        let disk1_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let disk1 = membership
            .remove_by_uuid(&disk1_uuid)
            .expect("disk1 fixture member");
        membership
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                disk1,
            )
            .expect("replace disk1 fixture UUID");

        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1", // mapper already open
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mapper_open(
                "braid-disk1",
                "/dev/vda",
                "ffffffff-ffff-ffff-ffff-ffffffffffff",
            )
            .with_mapper_closed("braid-disk2");

        let result =
            open_and_mount_for_test(&runner, &fs, &config, &membership, None, false, "unlock");

        let err = result.expect_err("should fail on LUKS UUID mismatch even with open mapper");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("111111"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }

    /// Intent: When cryptsetup open fails with a non-auth exit code (e.g. exit 4,
    /// device not found), the error must propagate as-is — not be rewritten as a
    /// single-passphrase invariant violation.
    ///
    /// Why: The mount helper previously replaced all open failures with the
    /// invariant message, masking non-auth causes like device disappearance.
    ///
    /// Scenario: 2-disk RAID1, passphrase verified against disk1, disk2 disappears
    /// (hot-unplug) before cryptsetup open runs.
    #[test]
    fn mount_non_auth_open_failure_propagates_passphrase() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let runner = base_two_disk_runner()
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw("cryptsetup open"),
            )
            // disk2 disappears → exit 4 (ENODEV)
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
                err_raw(
                    "cryptsetup open",
                    4,
                    "Device /dev/disk/by-id/virtio-disk2 does not exist.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 disappears");
        let msg = err.to_string();
        assert!(
            msg.contains("device not found"),
            "non-auth failure should propagate original hint, got: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "non-auth failure should not be rewritten as invariant violation, got: {msg}"
        );
    }

    /// Intent: When cryptsetup open with keyfile fails with a non-auth exit code
    /// (e.g. exit 4), the error must propagate as-is.
    ///
    /// Why: Same masking bug as the passphrase path — all keyfile open failures
    /// were rewritten as invariant violations.
    ///
    /// Scenario: 2-disk RAID1 with keyfile unlock, disk2 disappears before open.
    #[test]
    fn mount_non_auth_open_failure_propagates_keyfile() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }

        let runner = base_two_disk_runner()
            // verify keyfile against disk1 → success
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                ok_raw("cryptsetup open --test-passphrase"),
            )
            // open disk1 with keyfile → success
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                    key_file_path: kf.path().display().to_string(),
                },
                ok_raw("cryptsetup open"),
            )
            // disk2 disappears → exit 4 (ENODEV)
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                    key_file_path: kf.path().display().to_string(),
                },
                err_raw(
                    "cryptsetup open",
                    4,
                    "Device /dev/disk/by-id/virtio-disk2 does not exist.",
                ),
            );

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 disappears");
        let msg = err.to_string();
        assert!(
            msg.contains("device not found"),
            "non-auth failure should propagate original hint, got: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "non-auth failure should not be rewritten as invariant violation, got: {msg}"
        );
    }

    // --- explain_open_failure pure-helper tests ---
    //
    // These target the classification logic directly. The mount-level
    // integration tests further down prove the call sites are wired up
    // correctly; these tests prove the helper itself picks the right
    // branch for each header state.

    /*
     * Intent: Unreadable overrides whatever cryptsetup originally reported.
     * Why it exists: the whole point of probing is that header corruption
     *   should win over exit-code interpretation — an exit 2 from
     *   cryptsetup open should not surface "wrong passphrase" when the
     *   header is actually gone. Also pins the cross-command invariant
     *   that no message references local /var/lib/braid/luks-headers/.
     * Scenario: disk2's LUKS magic has been wiped by a misdirected dd,
     *   and cryptsetup open (unsurprisingly) also fails.
     */
    #[test]
    fn explain_open_failure_unreadable_overrides_fallback() {
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xDEAD",
            luks::LuksHeaderState::Unreadable,
            "some original summary",
            arbitrary_fallback(),
        );
        let msg = err.to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(msg.contains("off-system"), "missing 'off-system': {msg}");
        assert!(
            !msg.contains("ARBITRARY FALLBACK TEXT"),
            "unreadable branch must override fallback: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: Damaged overrides fallback and suggests cryptsetup repair
     *   with a safe-backup warning.
     * Why it exists: damaged metadata is recoverable via cryptsetup repair,
     *   but repair mutates the header so the user MUST back up first. This
     *   is the pairing that makes the suggestion safe to follow.
     * Scenario: disk2's LUKS2 metadata was corrupted but the magic bytes
     *   survived, so cryptsetup open fails on keyslot validation.
     */
    #[test]
    fn explain_open_failure_damaged_overrides_fallback() {
        let device = "/dev/disk/by-id/wwn-0xCAFE";
        let err = explain_open_failure(
            "disk2",
            device,
            luks::LuksHeaderState::Damaged,
            "some original summary",
            arbitrary_fallback(),
        );
        let msg = err.to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("metadata damaged"),
            "missing 'metadata damaged': {msg}"
        );
        assert!(
            msg.contains(&format!("cryptsetup repair --type luks2 {device}")),
            "missing repair command with device path: {msg}"
        );
        assert!(
            msg.contains("safe backup"),
            "missing safe-backup warning: {msg}"
        );
        assert!(
            !msg.contains("ARBITRARY FALLBACK TEXT"),
            "damaged branch must override fallback: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: Ok uses the caller's fallback verbatim.
     * Why it exists: when the header is proven intact, the failure really
     *   is about the passphrase/invariant/device — the existing error
     *   messages are correct and must be preserved untouched.
     * Scenario: user types the wrong passphrase on a healthy pool.
     */
    #[test]
    fn explain_open_failure_ok_uses_fallback_verbatim() {
        let fallback_text = "wrong passphrase (verified against disk1)";
        let err = explain_open_failure(
            "disk1",
            "/dev/disk/by-id/wwn-0xOK",
            luks::LuksHeaderState::Ok,
            "original summary not used in Ok branch",
            MountError::Failed(fallback_text.into()),
        );
        assert_eq!(err.to_string(), fallback_text);
    }

    /*
     * Intent: Ok preserves the single-passphrase-invariant message exactly
     *   for the exit-2-on-subsequent-disk scenario.
     * Why it exists: the invariant-violation message is the subtlest existing
     *   message, and misrouting it would either break a valid warning or
     *   misdiagnose corruption as an invariant violation. This is a specific
     *   regression test pinning the fallback path.
     * Scenario: disk2 rejects a verified passphrase but its header is intact,
     *   indicating real external LUKS manipulation.
     */
    #[test]
    fn explain_open_failure_ok_preserves_invariant_message() {
        let fallback_text = "failed to open disk 'disk2': passphrase was verified against 'disk1' but \
             rejected here -- wrong passphrase or permission denied (EPERM). If the \
             passphrase is correct, the single-passphrase invariant may be violated \
             by external LUKS manipulation";
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xOK",
            luks::LuksHeaderState::Ok,
            "unused",
            MountError::Failed(fallback_text.into()),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("single-passphrase invariant"),
            "Ok branch must preserve invariant-violation text: {msg}"
        );
    }

    // --- integration tests for enrichment wiring in open_and_mount_pool ---
    //
    // These prove the four call sites pass the right arguments into
    // `explain_open_failure`. The pure helper tests (above) cover each
    // classification branch; these cover the wiring.

    // Intent: Mount failure after two successful opens reports both mappers
    // as cleanup-owned and cleanup forgets those mapper paths before close.
    // Why it exists: fail-closed unlock depends on preserving the primary
    // mount error while still giving callers the exact mappers this command
    // opened.
    // Scenario: two closed LUKS members unlock, btrfs scan succeeds, mount
    // fails before the pool comes online.
    #[test]
    fn unlock_failure_after_two_opens_closes_both_after_scoped_forget() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let runner = direct_two_disk_open_runner()
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mount", 32, "wrong fs type"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk1".into()),
                },
                ok_raw("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk2".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let failure = execute_unlock_and_mount(&runner, &fs, &config, &plan, &test_passphrase())
            .expect_err("mount should fail");
        assert!(
            failure.error.to_string().starts_with("mount failed"),
            "primary error should be mount failure: {}",
            failure.error
        );
        assert_eq!(
            failure.opened_mappers,
            vec![
                MapperName("braid-disk1".into()),
                MapperName("braid-disk2".into()),
            ]
        );

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            close_opened_mappers(&runner, &NoopSleeper, &fs, &failure.opened_mappers, false)
                .unwrap();
        });
        assert!(
            captured.contains("cleanup: closed LUKS mappers opened by this command."),
            "missing cleanup success summary: {captured:?}"
        );

        let requests = runner.requests();
        let forget_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsDeviceScanForget { .. }))
            .expect("missing forget");
        let close_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .expect("missing close");
        assert!(forget_pos < close_pos, "forget must precede close");
    }

    // Intent: Btrfs scan failure after successful opens carries the same
    // opened-mapper cleanup set as mount failure.
    // Why it exists: btrfs device scan is post-open and pre-mount, so a
    // failure there would otherwise strand newly-opened LUKS mappers.
    // Scenario: both disks open, but `btrfs device scan` returns non-zero.
    #[test]
    fn unlock_scan_failure_reports_opened_mappers_for_cleanup() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let runner = direct_two_disk_open_runner().with_output(
            CmdRequest::BtrfsDeviceScanAll,
            err_raw("btrfs device scan", 1, "scan failed"),
        );

        let failure = execute_unlock_and_mount(&runner, &fs, &config, &plan, &test_passphrase())
            .expect_err("scan should fail");

        assert!(
            failure
                .error
                .to_string()
                .starts_with("btrfs device scan failed"),
            "primary error should be scan failure: {}",
            failure.error
        );
        assert_eq!(
            failure.opened_mappers,
            vec![
                MapperName("braid-disk1".into()),
                MapperName("braid-disk2".into()),
            ]
        );
    }

    // Intent: A mapper that becomes already-owned at execute time is not
    // included in the fail-closed cleanup set.
    // Why it exists: plan.to_unlock is not authoritative after planning; the
    // LUKS helper's OpenOutcome is the ownership boundary.
    // Scenario: disk1 was closed during planning but manually opened before
    // execution, while disk2 is opened by this command.
    #[test]
    fn already_owned_execute_race_is_filtered_from_cleanup_set() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let runner = MockRunner::default()
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_mapper_open(
                "braid-disk1",
                "/dev/vdb",
                "11111111-1111-1111-1111-111111111111",
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: "11111111-1111-1111-1111-111111111111\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_mapper_closed("braid-disk2")
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                err_raw("btrfs device scan", 1, "scan failed"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk2".into()],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk2".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let failure = execute_unlock_and_mount(&runner, &fs, &config, &plan, &test_passphrase())
            .expect_err("scan should fail");
        assert_eq!(
            failure.opened_mappers,
            vec![MapperName("braid-disk2".into())]
        );

        close_opened_mappers(&runner, &NoopSleeper, &fs, &failure.opened_mappers, false).unwrap();
        assert!(
            !runner.requests().iter().any(
                |r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk1")
            ),
            "must not close already-owned disk1"
        );
    }

    // Intent: If disk2 fails to open after disk1 opened, cleanup is scoped to
    // disk1 and the disk2 open failure remains primary.
    // Why it exists: a mid-open failure is the narrowest path where the open
    // loop must preserve both ownership and error precedence.
    // Scenario: disk1 opens, disk2 rejects during `cryptsetup open`.
    #[test]
    fn second_open_failure_preserves_error_and_cleans_first_open() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk2");
        let (dump_req, dump_out) = luks_dump_text_ok("/dev/disk/by-id/virtio-disk2");
        let runner = direct_two_disk_open_runner()
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                err_raw("cryptsetup open", 1, "open failed"),
            )
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out)
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk1".into()],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk1".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let failure = execute_unlock_and_mount(&runner, &fs, &config, &plan, &test_passphrase())
            .expect_err("disk2 open should fail");
        assert!(
            failure.error.to_string().contains("disk2"),
            "primary error should name disk2: {}",
            failure.error
        );
        assert_eq!(
            failure.opened_mappers,
            vec![MapperName("braid-disk1".into())]
        );

        close_opened_mappers(&runner, &NoopSleeper, &fs, &failure.opened_mappers, false).unwrap();
    }

    // Intent: Cleanup attempts every opened mapper even when one close stays
    // busy through all retries.
    // Why it exists: a failed cleanup for disk1 must not strand disk2 without
    // even trying to close it.
    // Scenario: disk1 close returns exit 5 for all retries; disk2 closes.
    #[test]
    fn cleanup_busy_close_attempts_later_mappers_and_reports_guidance() {
        let fs = direct_two_disk_fs_with_mappers();
        let opened = vec![
            MapperName("braid-disk1".into()),
            MapperName("braid-disk2".into()),
        ];
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk1".into()),
                },
                err_raw("cryptsetup close", 5, "busy"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk2".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let err = close_opened_mappers(&runner, &NoopSleeper, &fs, &opened, false)
                .expect_err("cleanup should report busy disk1");
            assert!(
                err.to_string().contains("device busy"),
                "expected busy cleanup error, got: {err}"
            );
        });

        assert!(
            captured.contains("cleanup failed: one or more LUKS mappers opened by this command"),
            "missing cleanup failure guidance: {captured:?}"
        );
        assert!(
            runner.requests().iter().any(
                |r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2")
            ),
            "cleanup must still attempt disk2"
        );
    }

    // Intent: Credential verification rejection with zero opens does not run
    // forget, close, or trailing cleanup summary.
    // Why it exists: wrong-passphrase failures must not emit cleanup noise or
    // attempt to close operator-owned mappers.
    // Scenario: disk1 rejects the passphrase during the all-disk verification
    // pass before any `cryptsetup open` command runs.
    #[test]
    fn wrong_passphrase_zero_open_cleanup_is_noop() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk1");
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");
        let (dump_req, dump_out) = luks_dump_text_ok("/dev/disk/by-id/virtio-disk1");
        let runner = MockRunner::default()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);

        let failure = execute_unlock_and_mount(&runner, &fs, &config, &plan, &test_passphrase())
            .expect_err("credential verify should fail");
        assert!(
            failure.opened_mappers.is_empty(),
            "wrong passphrase should not report opened mappers"
        );

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            close_opened_mappers(&runner, &NoopSleeper, &fs, &failure.opened_mappers, false)
                .unwrap();
        });
        assert_eq!(captured, "", "zero-open cleanup should be silent");
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceScanForget { .. } | CmdRequest::CryptsetupClose { .. }
            )),
            "zero-open cleanup must not forget or close"
        );
    }

    // Intent: Keyfile unlock uses the same opened-mapper cleanup tracking as
    // passphrase unlock.
    // Why it exists: a passphrase-only fix would leave auto-unlock failures
    // able to strand mappers.
    // Scenario: two keyfile opens succeed, then btrfs scan fails.
    #[test]
    fn keyfile_post_open_failure_reports_opened_mappers_for_cleanup() {
        let config = test_config();
        let fs = direct_two_disk_fs_with_mappers();
        let plan = direct_two_disk_plan();
        let keyfile = tempfile::NamedTempFile::new().unwrap();
        let keyfile_path = keyfile.path().display().to_string();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    key_file_path: keyfile_path.clone(),
                },
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    key_file_path: keyfile_path.clone(),
                },
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName("braid-disk1".into()),
                    key_file_path: keyfile_path.clone(),
                },
                ok_raw("cryptsetup open"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                    key_file_path: keyfile_path,
                },
                ok_raw("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                err_raw("btrfs device scan", 1, "scan failed"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk1".into()),
                },
                ok_raw("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk2".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let failure = execute_unlock_and_mount(
            &runner,
            &fs,
            &config,
            &plan,
            &OpenCredential::KeyFile(keyfile.path().to_path_buf()),
        )
        .expect_err("scan should fail");

        assert_eq!(
            failure.opened_mappers,
            vec![
                MapperName("braid-disk1".into()),
                MapperName("braid-disk2".into()),
            ]
        );

        close_opened_mappers(&runner, &NoopSleeper, &fs, &failure.opened_mappers, false)
            .expect("cleanup should close keyfile-opened mappers");

        let requests = runner.requests();
        let expected_forget_devices = vec![
            "/dev/mapper/braid-disk1".to_owned(),
            "/dev/mapper/braid-disk2".to_owned(),
        ];
        let forget_pos = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScanForget { devices }
                        if devices == &expected_forget_devices
                )
            })
            .expect("cleanup should issue scoped btrfs device scan --forget");
        let close_positions: Vec<_> = requests
            .iter()
            .enumerate()
            .filter_map(|(idx, request)| match request {
                CmdRequest::CryptsetupClose { mapper } => Some((idx, mapper.as_str())),
                _ => None,
            })
            .collect();

        assert_eq!(
            close_positions,
            vec![
                (forget_pos + 1, "braid-disk1"),
                (forget_pos + 2, "braid-disk2"),
            ],
            "cleanup should forget the opened mapper paths before closing both mappers"
        );
    }

    // Intent: Cleanup warns and continues when scoped `btrfs device scan
    // --forget` fails.
    // Why it exists: forget is a stale-cache mitigation, not a reason to skip
    // closing mappers opened by this command.
    // Scenario: forget returns non-zero, but both close calls succeed.
    #[test]
    fn cleanup_forget_failure_warns_and_still_closes_all_mappers() {
        let fs = direct_two_disk_fs_with_mappers();
        let opened = vec![
            MapperName("braid-disk1".into()),
            MapperName("braid-disk2".into()),
        ];
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                err_raw("btrfs device scan --forget", 1, "forget failed"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk1".into()),
                },
                ok_raw("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName("braid-disk2".into()),
                },
                ok_raw("cryptsetup close"),
            );

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            close_opened_mappers(&runner, &NoopSleeper, &fs, &opened, false).unwrap();
        });

        assert!(
            captured.contains("btrfs device scan --forget failed"),
            "missing forget warning: {captured:?}"
        );
        assert!(
            captured.contains("cleanup: closed LUKS mappers opened by this command."),
            "successful close summary should still print: {captured:?}"
        );
        let closes = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(closes, 2, "forget failure must not skip closes");
    }

    /*
     * Intent: When verify_passphrase fails on disk1 AND disk1's LUKS header
     *   is unreadable, the error tells the user to restore from an off-system
     *   backup — not that the passphrase is wrong.
     * Why it exists: a fully-wiped disk1 header causes cryptsetup
     *   --test-passphrase to fail, which looks exactly like a wrong
     *   passphrase at the boolean level. Without probing, the user gets
     *   pointed at the wrong problem. This is the Ultraplan Medium
     *   coverage for the passphrase verify-step wiring.
     * Scenario: disk1's header was clobbered by a misdirected dd; the
     *   user tries to unlock with a perfectly correct passphrase.
     */
    #[test]
    fn unlock_passphrase_verify_fails_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk1");
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk1");

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output(is_req, is_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk1"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("wrong passphrase"),
            "unreadable header must not blame passphrase: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
    }

    /*
     * Intent: A configured pool member with damaged LUKS2 metadata must
     *   fail at the gateway probe (probe_config_disk → luksDump exit
     *   non-zero → ProbeError::Parse) BEFORE any unlock attempt runs.
     * Why it exists: braid's gateway invariant says probe_config_disk is
     *   the single source of truth for "is this configured disk usable?".
     *   The previous code path enriched a verify-passphrase failure with
     *   probe_luks_header's Damaged classification, but that diagnostic
     *   path is now unreachable for configured disks because the gateway
     *   catches damaged metadata first. This test pins the gateway
     *   behavior so a future regression that loosens probe_config_disk
     *   (e.g., a CommandFailed swallow) is caught.
     * Scenario: disk1's LUKS2 keyslot metadata is corrupted; the user
     *   tries to unlock via keyfile.
     */
    #[test]
    fn unlock_damaged_luks2_metadata_fails_at_gateway() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }

        let (dump_req, dump_out) = luks_dump_text_fail("/dev/disk/by-id/virtio-disk1");
        let runner = base_two_disk_runner().with_output(dump_req, dump_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected gateway failure").to_string();
        // The gateway propagates the cryptsetup luksDump error verbatim;
        // we don't try to fabricate "metadata damaged" guidance here.
        // The user gets enough to investigate (cryptsetup, luksDump, the
        // verbatim stderr), and `cryptsetup repair --type luks2` is the
        // documented recovery they can run themselves.
        assert!(
            msg.contains("luksDump"),
            "gateway error must surface luksDump as the failing command: {msg}"
        );
        assert!(
            msg.contains("Cannot read LUKS header metadata"),
            "gateway error must include cryptsetup stderr: {msg}"
        );
        assert!(
            !msg.contains("wrong keyfile"),
            "gateway must reject before keyfile verification runs: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
    }

    /*
     * Intent: When verify_passphrase fails on disk1 AND disk1's LUKS header
     *   is intact, the existing "wrong passphrase" message is preserved.
     * Why it exists: the intact-header fallback must still wire through
     *   the enrichment path unchanged — any regression would break the
     *   most common wrong-passphrase UX.
     * Scenario: user types a wrong passphrase on a healthy pool.
     */
    #[test]
    fn unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = test_passphrase_fail("/dev/disk/by-id/virtio-disk1");
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");
        let (dump_req, dump_out) = luks_dump_text_ok("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"wrongpass".to_vec(), tp_out)
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::Passphrase(Passphrase::from_zeroizing(
                Zeroizing::new("wrongpass".to_owned()),
            ))),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(
            msg.contains("wrong passphrase (rejected by disk1)"),
            "intact header must preserve wrong-passphrase message: {msg}"
        );
        assert!(
            !msg.contains("header unreadable"),
            "intact header must not route to unreadable guidance: {msg}"
        );
    }

    /*
     * Intent: When ensure_luks_open fails with exit 2 on disk2 AND the
     *   header probe itself fails to execute, the error must surface
     *   diagnostic uncertainty — NOT confidently blame the single-
     *   passphrase invariant.
     * Why it exists: this is the Ultraplan High finding coverage at the
     *   integration level. With cryptsetup missing from PATH (or any
     *   runner error during the probe), the old fallback would have
     *   silently reverted to the "single-passphrase invariant may be
     *   violated" text, which is exactly the misdiagnosis this plan is
     *   meant to stop. The probe mocks are deliberately not seeded so
     *   MockRunner returns CmdError::MissingMock, which classifies as
     *   ProbeFailed.
     * Scenario: disk2 rejects a verified passphrase (could be real
     *   invariant violation OR corruption), and cryptsetup is missing
     *   from PATH on the machine.
     */
    #[test]
    fn unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = (
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            ok_raw("cryptsetup open --test-passphrase"),
        );
        let (open1_req, open1_out) = (
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                mapper: MapperName("braid-disk1".into()),
            },
            ok_raw("cryptsetup open"),
        );
        let (open2_req, open2_out) = (
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                mapper: MapperName("braid-disk2".into()),
            },
            err_raw(
                "cryptsetup open",
                2,
                "No key available with this passphrase.",
            ),
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        // Deliberately NOT seeding CryptsetupIsLuks on disk2 → MissingMock
        // → ProbeFailed. This is the point of the test.
        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output_stdin(open1_req, b"testpass".to_vec(), open1_out)
            .with_output_stdin(open2_req, b"testpass".to_vec(), open2_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk2"), "missing failing disk: {msg}");
        assert!(
            msg.contains("diagnosis could not be completed"),
            "missing 'diagnosis could not be completed': {msg}"
        );
        // Load-bearing: the old design would leak the invariant-violation
        // text here; the new design must not.
        assert!(
            !msg.contains("single-passphrase invariant"),
            "ProbeFailed must not blame invariant: {msg}"
        );
    }

    /*
     * Intent: When ensure_luks_open_with_key_file fails with a non-2 exit
     *   on disk2 AND disk2's LUKS header is unreadable, the error emits
     *   the off-system backup guidance.
     * Why it exists: covers the keyfile-open-loop wiring with the
     *   Unreadable classification. Also verifies the fallback-construction
     *   path for non-exit-2 cases (the `_` arm in the match).
     * Scenario: a keyfile-driven auto-unlock where disk2's header has
     *   been wiped; cryptsetup open reports a generic failure (exit 1).
     */
    #[test]
    fn unlock_keyfile_open_exit_nonzero_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }
        let kf_path = kf.path().display().to_string();

        let (tk_req, tk_out) = (
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                key_file_path: kf_path.clone(),
            },
            ok_raw("cryptsetup open --test-passphrase"),
        );
        let (tk2_req, tk2_out) = (
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                key_file_path: kf_path.clone(),
            },
            ok_raw("cryptsetup open --test-passphrase"),
        );
        let (open1_req, open1_out) = (
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                mapper: MapperName("braid-disk1".into()),
                key_file_path: kf_path.clone(),
            },
            ok_raw("cryptsetup open"),
        );
        let (open2_req, open2_out) = (
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                mapper: MapperName("braid-disk2".into()),
                key_file_path: kf_path,
            },
            err_raw("cryptsetup open", 1, "Cannot read LUKS header"),
        );
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk2");

        let runner = base_two_disk_runner()
            .with_output(tk_req, tk_out)
            .with_output(tk2_req, tk2_out)
            .with_output(open1_req, open1_out)
            .with_output(open2_req, open2_out)
            .with_output(is_req, is_out);

        let result = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        );

        let msg = result.expect_err("expected failure").to_string();
        assert!(msg.contains("disk2"), "missing disk name: {msg}");
        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: ProbeFailed emits a distinct "diagnosis could not be completed"
     *   message and does NOT fall back to passphrase or invariant wording.
     * Why it exists: this is the executable form of an Ultraplan High
     *   finding. If the probe itself errors (e.g. cryptsetup missing from
     *   PATH), a naive design would route to the fallback — which is exactly
     *   the "wrong passphrase" or "single-passphrase invariant" text this
     *   enrichment is meant to stop. Probe-execution failure must surface
     *   as uncertainty, not as confident blame of the passphrase.
     * Scenario: cryptsetup binary is missing from PATH while a disk has
     *   also legitimately failed to open for an unknown reason.
     */
    #[test]
    fn explain_open_failure_probe_failed_emits_diagnosis_incomplete() {
        let fallback_text = "failed to open disk 'disk2': passphrase was verified against 'disk1' but \
             rejected here. If the passphrase is correct, the single-passphrase \
             invariant may be violated by external LUKS manipulation";
        let err = explain_open_failure(
            "disk2",
            "/dev/disk/by-id/wwn-0xDEAD",
            luks::LuksHeaderState::ProbeFailed("simulated probe error".into()),
            "cryptsetup open rejected on 'disk2' despite verified passphrase on 'disk1'",
            MountError::Failed(fallback_text.into()),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("diagnosis could not be completed"),
            "missing 'diagnosis could not be completed': {msg}"
        );
        assert!(
            msg.contains("simulated probe error"),
            "missing probe error surface: {msg}"
        );
        assert!(
            msg.contains("cryptsetup open rejected on 'disk2'"),
            "missing original summary: {msg}"
        );
        // The new wording must stay neutral about the underlying cause —
        // the helper is used for non-auth failures too (device not found,
        // busy, generic I/O), so it cannot narrow to "passphrase".
        assert!(
            msg.contains("Cannot determine whether this failure"),
            "missing neutral 'Cannot determine' framing: {msg}"
        );
        // Load-bearing negative assertions: probe-failed must NOT leak the
        // misleading passphrase/invariant wording from the fallback, and
        // must NOT narrow the cause to "passphrase" (the review finding
        // this test's wording pins).
        assert!(
            !msg.contains("wrong passphrase"),
            "ProbeFailed must not blame passphrase: {msg}"
        );
        assert!(
            !msg.contains("single-passphrase invariant"),
            "ProbeFailed must not blame invariant: {msg}"
        );
        assert!(
            !msg.contains("passphrase problem"),
            "ProbeFailed must not narrow cause to 'passphrase problem' — the \
             helper is called from non-auth failure sites too: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    // ---- VerifyOutcome non-auth routing tests ---------------------------
    //
    // Pin the behavior split added to both verify callsites: on a non-auth
    // verify exit, the outcome branches on LuksHeaderState.
    //   - LuksHeaderState::Ok -> MountError::Luks(OpenFailed) with the raw
    //     exit code/hint (no "wrong passphrase" narrative).
    //   - LuksHeaderState::Unreadable -> off-system-backup guidance.
    // Both branches are covered for each credential type so a revert at
    // either callsite fails at least one test.

    /*
     * Intent: a non-auth verify exit (EBUSY) with a healthy LUKS header
     *   surfaces as MountError::Luks(OpenFailed { exit_code: 5, .. }) and
     *   does NOT route into the "wrong passphrase" fallback.
     * Why it exists: this is the central regression probe for the
     *   misdiagnosis bug -- before the VerifyOutcome refactor, every
     *   non-zero verify exit collapsed to Ok(false), reaching the
     *   LuksHeaderState::Ok fallback and telling users their passphrase
     *   was wrong when the real cause was a busy device.
     * Scenario: a stale dm-crypt mapper from a prior unlock attempt is
     *   still holding the backing device open; the user tries `braid
     *   unlock` with a perfectly correct passphrase.
     */
    #[test]
    fn unlock_passphrase_verify_exit_5_ok_header_surfaces_open_failed() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = (
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            err_raw(
                "cryptsetup open --test-passphrase",
                5,
                "Device /dev/dm-0 already exists.",
            ),
        );
        // Healthy header: isLuks ok, luksDumpText ok (base runner seeds the dump).
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output(is_req, is_out);

        let err = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        )
        .expect_err("expected failure");

        match err {
            MountError::Luks(LuksError::OpenFailed {
                exit_code, hint, ..
            }) => {
                assert_eq!(exit_code, 5);
                assert_eq!(hint, "device is already open or busy");
            }
            other => panic!(
                "expected MountError::Luks(LuksError::OpenFailed {{ exit_code: 5, .. }}), got: {other}"
            ),
        }
    }

    /*
     * Intent: a non-auth verify exit (generic failure, exit 1) with an
     *   unreadable LUKS header emits the off-system-backup guidance --
     *   not a raw "generic failure" string.
     * Why it exists: this pins the high-severity review concern. Header
     *   diagnosis must remain reachable when verify hits a non-auth exit;
     *   otherwise a wiped header path regresses to a bare cryptsetup
     *   message that does not tell the user what to do.
     * Scenario: a misdirected `dd` wiped disk1's LUKS header; cryptsetup
     *   --test-passphrase on the raw device now returns a generic
     *   failure because there is no LUKS structure to test against.
     */
    #[test]
    fn unlock_passphrase_verify_exit_1_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (tp_req, tp_out) = (
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            err_raw("cryptsetup open --test-passphrase", 1, "generic failure"),
        );
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output_stdin(tp_req, b"testpass".to_vec(), tp_out)
            .with_output(is_req, is_out);

        let msg = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(test_passphrase()),
            false,
            "unlock",
        )
        .expect_err("expected failure")
        .to_string();

        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("wrong passphrase"),
            "unreadable header must not blame passphrase: {msg}"
        );
        assert!(
            !msg.contains("generic failure"),
            "unreadable header must not leak the raw cryptsetup hint: {msg}"
        );
    }

    /*
     * Intent: keyfile-path mirror of the passphrase exit-5 regression
     *   probe. A busy backing device during keyfile verify surfaces as
     *   MountError::Luks(OpenFailed { exit_code: 5, .. }), not as
     *   "wrong keyfile".
     * Why it exists: the keyfile callsite had the same silent-bool bug
     *   as the passphrase callsite; both need independent regression
     *   coverage so a revert at either one fails at least one test.
     * Scenario: auto-unlock via keyfile, but a prior unlock attempt left
     *   a stale mapper holding the backing device busy.
     */
    #[test]
    fn unlock_keyfile_verify_exit_5_ok_header_surfaces_open_failed() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }
        let kf_path = kf.path().display().to_string();

        let (tk_req, tk_out) = (
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                key_file_path: kf_path,
            },
            err_raw(
                "cryptsetup open --test-passphrase --key-file",
                5,
                "Device /dev/dm-0 already exists.",
            ),
        );
        let (is_req, is_out) = is_luks_ok("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output(tk_req, tk_out)
            .with_output(is_req, is_out);

        let err = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        )
        .expect_err("expected failure");

        match err {
            MountError::Luks(LuksError::OpenFailed {
                exit_code, hint, ..
            }) => {
                assert_eq!(exit_code, 5);
                assert_eq!(hint, "device is already open or busy");
            }
            other => panic!(
                "expected MountError::Luks(LuksError::OpenFailed {{ exit_code: 5, .. }}), got: {other}"
            ),
        }
    }

    /*
     * Intent: keyfile-path mirror of the passphrase exit-1 +
     *   unreadable-header test. A wiped header during keyfile verify
     *   routes to off-system-backup guidance, not a raw hint.
     * Why it exists: same as the passphrase variant -- the keyfile
     *   callsite needs independent coverage for the header-diagnosis
     *   routing on non-auth verify exits.
     * Scenario: disk1's LUKS header was clobbered; an auto-unlock run
     *   attempts to verify the keyfile against the raw device and gets
     *   a generic failure because there is no header to test against.
     */
    #[test]
    fn unlock_keyfile_verify_exit_1_unreadable_header_emits_guidance() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = mount_fs(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let kf = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            kf.as_file().write_all(b"keydata").unwrap();
        }
        let kf_path = kf.path().display().to_string();

        let (tk_req, tk_out) = (
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                key_file_path: kf_path,
            },
            err_raw(
                "cryptsetup open --test-passphrase --key-file",
                1,
                "generic failure",
            ),
        );
        let (is_req, is_out) = is_luks_fail("/dev/disk/by-id/virtio-disk1");

        let runner = base_two_disk_runner()
            .with_output(tk_req, tk_out)
            .with_output(is_req, is_out);

        let msg = open_and_mount_for_test(
            &runner,
            &fs,
            &config,
            &membership,
            Some(OpenCredential::KeyFile(kf.path().to_path_buf())),
            false,
            "unlock",
        )
        .expect_err("expected failure")
        .to_string();

        assert!(
            msg.contains("header unreadable"),
            "missing 'header unreadable': {msg}"
        );
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("wrong keyfile"),
            "unreadable header must not blame keyfile: {msg}"
        );
        assert!(
            !msg.contains("generic failure"),
            "unreadable header must not leak the raw cryptsetup hint: {msg}"
        );
    }
}
