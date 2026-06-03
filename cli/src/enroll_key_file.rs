use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::mapper_name;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, probe_keyfile_enrollment,
    verify_credential_for_targets,
};
use crate::luks::{
    self, BackingPathResolver, KEYFILE_SIZE, KeySlotState, LUKS_SLOT_KEYFILE, LuksError,
    VerifyOutcome,
};
use crate::membership::PoolMembership;
use crate::parse::parse_cryptsetup_luks_uuid;
use crate::preflight;
use crate::preview::{
    self, NoteLevel, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote,
};
use crate::probe::{self, Filesystem};
use crate::secret::Passphrase;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{ByIdPath, ConfigDiskState, DiskName, LuksUuid, MountPoint};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum EnrollKeyFileError {
    #[error("{0}")]
    Validation(String),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("luks error: {0}")]
    Luks(#[from] LuksError),
    #[error("probe error: {0}")]
    Probe(#[from] crate::probe::ProbeError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiskEnrollAction {
    AlreadyEnrolled { name: DiskName, by_id: ByIdPath },
    NeedsEnroll { name: DiskName, by_id: ByIdPath },
}

/// Mode dispatch for `plan_single_disk_enrollment` (and the per-candidate
/// loop in `plan_enrollment`). The two modes share slot-1 conflict
/// detection but differ on whether the keyfile probe
/// (`luks::verify_key_file`) runs.
///
/// `GenerateNew` must skip the keyfile probe -- the keyfile does not exist
/// yet, so probing it would always fail with "Failed to open key file" and
/// abort enrollment before the file is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnrollmentPlanMode {
    /// User-supplied keyfile already on disk. Probe it on each candidate
    /// to support idempotent re-enroll, then check slot 1 on disks that
    /// don't already have it.
    ExistingKeyfile,
    /// `--generate`: keyfile does not exist yet. Skip the keyfile probe
    /// entirely and only check slot 1 on every candidate -- every disk
    /// is by definition `NeedsEnroll`.
    GenerateNew,
}

/// A present LUKS pool member that discovery validated as enrollable.
/// Carries the `uuid` discovery already proved equals the live header so
/// the execute-time re-probe (`reprobe_member_luks_uuid`) can compare
/// against the exact value discovery validated, rather than re-deriving
/// it from membership at the mutation boundary.
#[derive(Debug, Clone)]
pub struct EnrollmentCandidate {
    pub name: DiskName,
    pub by_id: ByIdPath,
    pub uuid: LuksUuid,
}
type EnrollmentCandidateDiscovery = (
    Vec<PreviewNote>,
    Result<Vec<EnrollmentCandidate>, EnrollKeyFileError>,
);

/// Discovery phase: iterate membership disks and collect present LUKS
/// candidates. Absent and non-LUKS disks become `PreviewNote::PerDisk`
/// Skip notes accumulated alongside the candidate list. The caller
/// routes notes to the appropriate channel (dry-run stdout via
/// `Preview::render`, real-run stderr prelude, or failure-path stderr
/// via `render_notes_for_stderr`). Probe errors propagate with any
/// notes accumulated so far; zero candidates after the loop is a
/// preserved-context failure.
fn discover_enrollment_candidates<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    membership: &PoolMembership,
    backing_path_resolver: &dyn BackingPathResolver,
) -> EnrollmentCandidateDiscovery {
    let mut notes: Vec<PreviewNote> = Vec::new();
    let mut candidates: Vec<EnrollmentCandidate> = Vec::new();

    for (expected_uuid, member) in membership.iter_by_name() {
        let name = &member.name;
        let probed = match probe::probe_config_disk(
            runner,
            fs,
            &member.name,
            &member.by_id,
            backing_path_resolver,
        ) {
            Ok(p) => p,
            Err(e) => return (notes, Err(e.into())),
        };
        match &probed.state {
            ConfigDiskState::Absent => {
                notes.push(PreviewNote::PerDisk {
                    name: name.as_str().to_owned(),
                    level: NoteLevel::Skip,
                    message: "not present".into(),
                });
            }
            ConfigDiskState::PresentNotLuks => {
                notes.push(PreviewNote::PerDisk {
                    name: name.as_str().to_owned(),
                    level: NoteLevel::Skip,
                    message: "not LUKS-formatted".into(),
                });
            }
            ConfigDiskState::PresentLuks { uuid, .. } => {
                if expected_uuid != uuid {
                    return (
                        notes,
                        Err(EnrollKeyFileError::Validation(
                            luks::format_luks_uuid_mismatch(
                                name.as_str(),
                                &member.by_id,
                                expected_uuid,
                                uuid,
                            ),
                        )),
                    );
                }
                candidates.push(EnrollmentCandidate {
                    name: name.clone(),
                    by_id: member.by_id.clone(),
                    uuid: expected_uuid.clone(),
                });
            }
        }
    }

    if candidates.is_empty() {
        return (
            notes,
            Err(EnrollKeyFileError::Validation(
                "no present LUKS disks found to enroll keyfile into".into(),
            )),
        );
    }

    (notes, Ok(candidates))
}

/// Slot-1 preflight: refuse to enroll if slot 1 is already occupied by
/// an unknown key. Same remediation regardless of mode.
fn check_slot_one_available<R: CommandRunner>(
    runner: &R,
    name: &DiskName,
    by_id: &ByIdPath,
) -> Result<(), EnrollKeyFileError> {
    let slot_state = luks::check_key_slot(runner, by_id.as_str(), LUKS_SLOT_KEYFILE)?;
    if slot_state == KeySlotState::Occupied {
        return Err(EnrollKeyFileError::Validation(format!(
            "slot 1 on {} ({}) is occupied by an unknown key. \
             Remove it first with `cryptsetup luksKillSlot {} 1` then re-run enrollment.",
            name, by_id, by_id
        )));
    }
    Ok(())
}

/// Execute-time re-probe of a candidate's live LUKS UUID, mirroring
/// `replace.rs#probe_existing_luks_new_target_uuid`. Decision-024 mandates
/// re-checking member identity at every mutation boundary: between
/// discovery and the `luksAddKey` mutation lies the operator-controlled
/// passphrase-prompt window, during which a disk could be swapped or
/// reformatted to a foreign LUKS container that happens to share the pool
/// passphrase. Comparing against the discovery-validated `expected` UUID
/// fails closed on every swap variant -- a different live UUID (including a
/// LUKS1 swap, which `luksUUID` still reads at exit 0) hits the mismatch
/// arm; an absent or non-LUKS device exits non-zero, so the parse error
/// (or a `run` failure) hits the fail-closed arm. No swap is silently
/// accepted.
///
/// A sub-second TOCTOU window remains between this probe and `luksAddKey`,
/// identical in kind to replace's probe -> open window; closing the
/// dominant human-scale passphrase-prompt window is the point. A per-disk
/// re-probe inside `apply_enrollment` would shave the negligible residual
/// at the cost of worse diagnostics and a guard split across two
/// functions, so it is out of scope.
fn reprobe_member_luks_uuid<R: CommandRunner>(
    runner: &R,
    name: &DiskName,
    by_id: &ByIdPath,
    expected: &LuksUuid,
) -> Result<(), EnrollKeyFileError> {
    let raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: by_id.as_str().to_owned(),
    })?;
    match parse_cryptsetup_luks_uuid(&raw) {
        Ok(parsed) if parsed.uuid == *expected => Ok(()),
        Ok(parsed) => Err(EnrollKeyFileError::Validation(
            luks::format_luks_uuid_mismatch(name.as_str(), by_id, expected, &parsed.uuid),
        )),
        Err(_) => Err(EnrollKeyFileError::Validation(format!(
            "disk '{}': cannot confirm LUKS UUID at {} before enrolling -- \
             device may have been swapped or disconnected after planning; \
             re-run `braid enroll`",
            name.as_str(),
            by_id
        ))),
    }
}

/// Per-disk enrollment classifier shared by `plan_enrollment` (the
/// standalone `braid enroll` planner) and the `add` / `replace` planners
/// when their `--enroll DIR` flag targets an already-LUKS disk. Owns the
/// `Some(kf) + already-LUKS target` decision in one place so no caller
/// can silently drop the keyfile -- the silent-drop bug this refactor
/// fixes.
///
/// No passphrase verification: callers verify credentials before invoking
/// this helper (standalone enroll batches up-front; add/replace verify
/// against the new disk's existing slot 0). Probe and slot-1 check are
/// non-mutating reads, so dry-run callers can use this too.
///
/// Mode dispatch:
/// - `ExistingKeyfile`: probe the keyfile (`[wait]/[ok]/[skip]` rows
///   emit via `probe_keyfile_enrollment`); `Authenticated` -> return
///   `AlreadyEnrolled`. `Rejected` falls through to slot-1 check.
/// - `GenerateNew`: skip the probe entirely (keyfile does not exist on
///   the standalone enroll `--generate` flow) and only run the slot-1
///   check.
///
/// Slot-1 outcomes:
/// - `Empty` -> `NeedsEnroll`.
/// - `Occupied` -> `Err(EnrollKeyFileError::Validation(..))` with the
///   canonical `cryptsetup luksKillSlot` remediation text.
pub(crate) fn plan_single_disk_enrollment<R: CommandRunner>(
    runner: &R,
    name: &DiskName,
    by_id: &ByIdPath,
    key_file_path: &Path,
    mode: EnrollmentPlanMode,
) -> Result<DiskEnrollAction, EnrollKeyFileError> {
    if let EnrollmentPlanMode::ExistingKeyfile = mode {
        // Only `Authenticated` means the keyfile is already installed.
        // `Rejected` is the normal "not yet enrolled" signal. Any other
        // non-zero exit (busy/missing/generic) propagates via `?` and
        // must NOT be silently treated as "not enrolled" -- doing so
        // would let the flow proceed to slot preflight on a device that
        // may not even be readable.
        let target = CredentialVerifyTarget {
            name: name.as_str().to_owned(),
            device: by_id.as_str().to_owned(),
        };
        let color_enabled = color_enabled_for_stderr();
        if matches!(
            probe_keyfile_enrollment(runner, &target, key_file_path, color_enabled, emit_status,)?,
            VerifyOutcome::Authenticated
        ) {
            return Ok(DiskEnrollAction::AlreadyEnrolled {
                name: name.clone(),
                by_id: by_id.clone(),
            });
        }
    }

    check_slot_one_available(runner, name, by_id)?;

    Ok(DiskEnrollAction::NeedsEnroll {
        name: name.clone(),
        by_id: by_id.clone(),
    })
}

/// Planning phase: verify passphrase, then classify each candidate disk.
/// Returns an immutable plan -- no mutations occur. Fails immediately on
/// wrong passphrase or slot-1 conflict.
///
/// Mode dispatch is delegated to `plan_single_disk_enrollment`.
fn plan_enrollment<R: CommandRunner>(
    runner: &R,
    candidates: &[EnrollmentCandidate],
    key_file_path: &Path,
    passphrase: &Passphrase,
    mode: EnrollmentPlanMode,
) -> Result<Vec<DiskEnrollAction>, EnrollKeyFileError> {
    let color_enabled = color_enabled_for_stderr();
    let verify_targets: Vec<CredentialVerifyTarget> = candidates
        .iter()
        .map(|c| CredentialVerifyTarget {
            name: c.name.as_str().to_owned(),
            device: c.by_id.as_str().to_owned(),
        })
        .collect();
    match verify_credential_for_targets(
        runner,
        &verify_targets,
        Credential::Passphrase(passphrase),
        color_enabled,
        |line| eprint!("{line}"),
    ) {
        Ok(()) => {}
        Err(CredentialVerifyError::Rejected { target }) => {
            return Err(EnrollKeyFileError::Validation(format!(
                "wrong passphrase on {}",
                target.name
            )));
        }
        Err(CredentialVerifyError::Luks { source, .. }) => {
            return Err(EnrollKeyFileError::Luks(source));
        }
    }

    // Passphrase has been verified against every candidate up-front, matching
    // sibling commands (mount/add/replace). The loop below only handles
    // mode-specific keyfile probing and slot-1 preflight via the shared helper.
    let mut plan = Vec::new();
    for c in candidates {
        let action = plan_single_disk_enrollment(runner, &c.name, &c.by_id, key_file_path, mode)?;
        if matches!(action, DiskEnrollAction::NeedsEnroll { .. }) {
            eprintln!("enroll: {} -- will add keyfile to slot 1", c.name);
        }
        plan.push(action);
    }

    Ok(plan)
}

/// Apply phase: execute mutations for NeedsEnroll items only.
fn apply_enrollment<R: CommandRunner>(
    runner: &R,
    plan: &[DiskEnrollAction],
    passphrase: &Passphrase,
    key_file_path: &Path,
    paths: &StatePaths,
) -> Result<(), EnrollKeyFileError> {
    let color_enabled = color_enabled_for_stderr();

    for action in plan {
        if let DiskEnrollAction::NeedsEnroll { name, by_id } = action {
            eprint!(
                "{}",
                status_line(
                    StatusTag::Wait,
                    color_enabled,
                    &format!("disk {name}: enrolling keyfile in slot 1..."),
                )
            );
            luks::enroll_key_file(runner, by_id.as_str(), passphrase, key_file_path)?;
            eprint!(
                "{}",
                status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {name}: keyfile enrolled in slot 1"),
                )
            );

            let mn = mapper_name(name);
            let backup_path =
                luks::backup_luks_header_post_mutation(runner, by_id.as_str(), &mn, paths)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());
        }
    }

    let enrolled = plan
        .iter()
        .filter(|a| matches!(a, DiskEnrollAction::NeedsEnroll { .. }))
        .count();
    let already = plan
        .iter()
        .filter(|a| matches!(a, DiskEnrollAction::AlreadyEnrolled { .. }))
        .count();
    eprintln!(
        "done: {} enrolled, {} already had keyfile",
        enrolled, already
    );
    Ok(())
}

/// Generate a random keyfile at `path` with mode 0o400.
/// Uses `create_new(true)` to atomically fail if the file already exists.
fn generate_key_file(path: &Path) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut rng = std::fs::File::open("/dev/urandom")?;
    let f = {
        let mut buf: Zeroizing<[u8; KEYFILE_SIZE]> = Zeroizing::new([0u8; KEYFILE_SIZE]);
        rng.read_exact(&mut buf[..])?;

        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(path)?;
        f.write_all(&buf[..])?;
        f
    };
    f.sync_all()?;

    // LUKS slots are mutated after this returns, so make the new directory
    // entry durable too; f.sync_all() alone does not guarantee the keyfile name
    // survives a pulled USB stick on filesystems without journaled metadata.
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "keyfile path has no parent directory",
        )
    })?;
    crate::state_io::sync_dir(parent)?;
    Ok(())
}

/// Compile dry-run steps from discovered candidates.
pub fn compile_enroll_steps(
    candidates: &[EnrollmentCandidate],
    key_file_path: &Path,
    generate: bool,
    paths: &StatePaths,
) -> Vec<Step> {
    let mut steps = Vec::new();

    if generate {
        steps.push(Step {
            risk: "safe",
            description: format!("generate keyfile -> {}", key_file_path.display()),
            commands: vec![],
        });
    }

    for c in candidates {
        let mn = mapper_name(&c.name);
        steps.push(Step {
            risk: "safe",
            description: format!("enroll keyfile -> LUKS slot 1 on {}", c.by_id),
            commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                device: c.by_id.as_str().to_owned(),
                key_file_path: key_file_path.display().to_string(),
            }],
        });
        let backup_path = luks::luks_header_backup_path(&paths.luks_headers_dir(), &mn);
        steps.push(Step {
            risk: "safe",
            description: format!("LUKS header backup -> {}", backup_path.display()),
            commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                device: c.by_id.as_str().to_owned(),
                backup_path: backup_path.display().to_string(),
            }],
        });
    }

    steps
}

pub struct EnrollKeyFileParams<'a> {
    pub membership: &'a PoolMembership,
    pub key_file_path: &'a Path,
    pub generate: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub dry_run: bool,
    pub paths: &'a StatePaths,
    /// Seam for resolving by-id paths and mapper backings during discovery.
    pub backing_path_resolver: &'a dyn BackingPathResolver,
}

/// Dry-run preview source of truth for `braid enroll` plus the
/// execute inputs pre-computed during planning. `notes` + `steps` are
/// both rendered by `preview()`; `execute()` renders `notes` to stderr
/// (using `STDERR_STYLE`) before any mutation.
#[derive(Debug)]
pub struct EnrollPlan {
    pub notes: Vec<PreviewNote>,
    /// Preview-only output artifact; empty on real-run plans, which
    /// re-plan from `candidates` after passphrase verification.
    pub steps: Vec<Step>,
    pub candidates: Vec<EnrollmentCandidate>,
    pub generate: bool,
}

impl EnrollPlan {
    /// Real-run and failure-path stderr both use `Plain` for `enroll`
    /// so today's pre-passphrase `skip: <name> not present` wording
    /// survives the migration byte-for-byte. `Preview::render` itself
    /// always uses `Bracketed`, so dry-run stdout wording differs --
    /// the "two products, two formats" call-out in the plan.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Plain;

    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.steps.clone(),
        }
    }

    pub fn execute<R: CommandRunner>(
        self,
        runner: &R,
        params: &EnrollKeyFileParams<'_>,
    ) -> Result<(), EnrollKeyFileError> {
        // Pre-passphrase: emit accumulated skip notes to stderr, same
        // wording as today's `skip: <name> ...` lines from the direct
        // `eprintln!` path.
        preview::emit_notes_to_stderr(&self.notes, Self::STDERR_STYLE);

        let passphrase = luks::read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

        // Decision-024 mutation-boundary re-check: the passphrase prompt is
        // an operator-controlled window in which a disk could be swapped or
        // reformatted. Re-probe every candidate's live LUKS UUID against the
        // value discovery validated before any keyfile lands in slot 1. Done
        // before the passphrase verify so a swap reports a clear UUID
        // mismatch rather than a misleading "wrong passphrase", and (in
        // --generate mode) before keyfile creation, which runs later.
        for c in &self.candidates {
            reprobe_member_luks_uuid(runner, &c.name, &c.by_id, &c.uuid)?;
        }

        let mode = if self.generate {
            EnrollmentPlanMode::GenerateNew
        } else {
            EnrollmentPlanMode::ExistingKeyfile
        };
        // `plan_enrollment` emits the `ok:` / `enroll:` status lines
        // directly on stderr -- intentionally scoped out of this
        // migration (they require a resolved passphrase and must not
        // leak before a wrong-passphrase error).
        let enrollment = plan_enrollment(
            runner,
            &self.candidates,
            params.key_file_path,
            &passphrase,
            mode,
        )?;

        if self.generate {
            validate_generated_keyfile_target(runner, params.key_file_path, true)?;
            generate_key_file(params.key_file_path)?;
            eprintln!("ok: generated {}", params.key_file_path.display());
        }

        let apply_result = apply_enrollment(
            runner,
            &enrollment,
            &passphrase,
            params.key_file_path,
            params.paths,
        );

        match apply_result {
            Ok(()) => Ok(()),
            Err(e) if self.generate => Err(EnrollKeyFileError::Validation(
                partial_generate_recovery_message(&e, params.key_file_path),
            )),
            Err(e) => Err(e),
        }
    }
}

/// Generate-mode apply failures need a retry command that reuses the
/// generated keyfile instead of orphaning an already-enrolled slot.
fn partial_generate_recovery_message(
    underlying: &EnrollKeyFileError,
    key_file_path: &Path,
) -> String {
    let dir = key_file_directory(key_file_path);
    format!(
        "{underlying}\n\n\
         The keyfile at {} was generated before this failure.\n\
         If enrollment did not complete on every disk, drop `--generate` and re-run:\n  \
         braid enroll {}",
        key_file_path.display(),
        dir.display()
    )
}

/// No-context preview-generation failure for bad `--key-file` paths.
/// Lives in `plan_enroll` so cmd-level code has a single planner
/// entry point. Failures here have no accumulated notes.
pub fn validate_key_file_path(
    key_file_path: &Path,
    generate: bool,
) -> Result<(), EnrollKeyFileError> {
    if generate {
        if key_file_path.exists() {
            let dir = key_file_directory(key_file_path);
            return Err(EnrollKeyFileError::Validation(format!(
                "braid.key already exists at {}.\n\
                 If a prior `--generate` run was interrupted, drop `--generate` and re-run \
                 `braid enroll {}` to finish enrolling the existing keyfile.\n\
                 Otherwise remove it manually if you want to generate a new one.",
                key_file_path.display(),
                dir.display()
            )));
        }
    } else {
        luks::validate_user_keyfile_path(key_file_path)?;
    }
    Ok(())
}

fn key_file_directory(key_file_path: &Path) -> &Path {
    key_file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn validate_generated_keyfile_target<R: CommandRunner>(
    runner: &R,
    key_file_path: &Path,
    recheck: bool,
) -> Result<(), EnrollKeyFileError> {
    let dir = key_file_directory(key_file_path);
    let dir_display = dir.display().to_string();

    let meta = match std::fs::metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(EnrollKeyFileError::Validation(format!(
                "keyfile directory does not exist: {dir_display}"
            )));
        }
        Err(e) => {
            return Err(EnrollKeyFileError::Validation(format!(
                "cannot read keyfile directory {dir_display}: {e}"
            )));
        }
    };
    if !meta.is_dir() {
        return Err(EnrollKeyFileError::Validation(format!(
            "keyfile target is not a directory: {dir_display}"
        )));
    }

    let mountpoint = runner.run(&CmdRequest::MountpointCheck {
        path: MountPoint(dir_display.clone()),
    })?;
    if mountpoint.exit_status != 0 {
        let message = if recheck {
            format!(
                "keyfile directory {dir_display} was a mount point at plan time but is no longer mounted -- the USB device may have been unmounted or disconnected during enrollment; remount and re-run braid enroll --generate"
            )
        } else {
            format!(
                "keyfile directory is not a mount point: {dir_display} -- mount the USB device there before running braid enroll --generate"
            )
        };
        return Err(EnrollKeyFileError::Validation(message));
    }

    validate_key_file_path(key_file_path, true)
}

/// Plan a `braid enroll` run. Owns the pending-op preflight,
/// keyfile-path validation, and pre-passphrase discovery. Per-disk
/// skip notes land on `EnrollPlan.notes` when discovery produces at
/// least one candidate, or on `PlanFailure::notes` when the
/// planner bails (e.g. no candidates, mid-loop probe error, dry-run
/// slot-1 conflict).
///
/// Dry-run classification: when `dry_run`, each candidate is routed
/// through `plan_single_disk_enrollment` so the preview matches the
/// real run's classification rules. `ExistingKeyfile` mode probes the
/// keyfile (passphrase-free, emits `[wait]/[ok]/[skip]` rows);
/// `Authenticated` becomes a `keyfile already enrolled` Skip note,
/// `Rejected` falls through to the slot-1 check. `GenerateNew` mode
/// (set by `--generate`) skips the keyfile probe entirely (the file
/// does not exist yet) and runs only the slot-1 check. Slot-1 conflicts
/// surface as `PlanFailure` with the same canonical `cryptsetup
/// luksKillSlot` recovery wording the real run uses. Real-run plans
/// (`dry_run = false`) leave `steps` empty because steps are preview-only;
/// execution defers classification to `plan_enrollment` after the
/// passphrase prompt. Takes the command's `EnrollKeyFileParams`;
/// `passphrase_stdin` / `passphrase_file` are intentionally unread here
/// because planning is pre-passphrase and execution consumes them after
/// the prompt.
pub fn plan_enroll<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &EnrollKeyFileParams<'_>,
) -> Result<EnrollPlan, PlanFailure<EnrollKeyFileError>> {
    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return Err(PlanFailure::empty(EnrollKeyFileError::Validation(msg)));
    }

    let key_file_validation = if params.generate {
        validate_generated_keyfile_target(runner, params.key_file_path, false)
    } else {
        validate_key_file_path(params.key_file_path, false)
    };
    if let Err(e) = key_file_validation {
        return Err(PlanFailure::empty(e));
    }

    let (mut notes, discovery) =
        discover_enrollment_candidates(runner, fs, params.membership, params.backing_path_resolver);
    let candidates = match discovery {
        Ok(c) => c,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };

    let steps = if params.dry_run {
        let mode = if params.generate {
            EnrollmentPlanMode::GenerateNew
        } else {
            EnrollmentPlanMode::ExistingKeyfile
        };
        let mut needs_enroll: Vec<EnrollmentCandidate> = Vec::with_capacity(candidates.len());
        for c in &candidates {
            match plan_single_disk_enrollment(runner, &c.name, &c.by_id, params.key_file_path, mode)
            {
                Ok(DiskEnrollAction::AlreadyEnrolled { name, .. }) => {
                    notes.push(PreviewNote::PerDisk {
                        name: name.as_str().to_owned(),
                        level: NoteLevel::Skip,
                        message: "keyfile already enrolled".into(),
                    });
                }
                // The action's name/by_id are clones of `c`'s; pushing the
                // original candidate is semantically identical and carries the
                // discovery-validated uuid forward. The dry-run path never
                // re-probes, so the carried uuid is inert here.
                Ok(DiskEnrollAction::NeedsEnroll { .. }) => {
                    needs_enroll.push(c.clone());
                }
                Err(e) => {
                    return Err(PlanFailure::with_notes(notes, e));
                }
            }
        }
        compile_enroll_steps(
            &needs_enroll,
            params.key_file_path,
            params.generate,
            params.paths,
        )
    } else {
        // Steps are a dry-run/preview-only artifact; real execution
        // re-plans from candidates after passphrase verification.
        Vec::new()
    };

    Ok(EnrollPlan {
        notes,
        steps,
        candidates,
        generate: params.generate,
    })
}

pub fn cmd_enroll_key_file<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &EnrollKeyFileParams<'_>,
) -> Result<(), EnrollKeyFileError> {
    let plan = match plan_enroll(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated skip notes render
            // to stderr before the error message, mirroring today's
            // `eprintln!("skip: ...")` + validation-error sequence on
            // the no-candidates path.
            preview::emit_notes_to_stderr(&notes, EnrollPlan::STDERR_STYLE);
            return Err(error);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner};
    use crate::test_fixtures::err_raw as enroll_err_raw;
    use crate::test_fixtures::{
        disk_member, enroll_add_keyfile_fail, enroll_add_keyfile_ok, enroll_by_id,
        enroll_discovery_two_disks, enroll_fs, enroll_luks_dump_slot1_empty,
        enroll_luks_dump_slot1_occupied, enroll_luks_uuid_not_luks, enroll_luks_uuid_ok,
        enroll_make_existing_keyfile, enroll_make_membership, enroll_mountpoint_fail,
        enroll_mountpoint_ok, enroll_passphrase, enroll_test_keyfile_fail, enroll_test_keyfile_ok,
        enroll_test_passphrase_fail, enroll_test_passphrase_ok, enroll_with_mountpoint_fail,
        enroll_with_mountpoint_ok, isolated_paths, mock_ok, test_uuid,
    };

    fn disk(name: &str) -> DiskName {
        DiskName::parse(name).expect("test disk name")
    }

    /// Build an `EnrollmentCandidate` for the `plan_enrollment` /
    /// `compile_enroll_steps` tests, which take candidates directly and
    /// never re-probe -- so the carried `uuid` is inert there and a fixed
    /// `test_uuid(500)` keeps these call sites terse. Tests that exercise
    /// the execute-time re-probe construct candidates inline with the
    /// specific uuid their `CryptsetupLuksUuid` mock returns.
    fn enroll_candidate(name: &str, by_id: &str) -> EnrollmentCandidate {
        EnrollmentCandidate {
            name: disk(name),
            by_id: enroll_by_id(by_id),
            uuid: test_uuid(500),
        }
    }

    // ---- plan_enroll discovery tests ----
    //
    // These tests exercise `plan_enroll(..., generate=true, ...)` because
    // `--generate` requires the keyfile path to NOT exist, so the temp
    // path (never created) satisfies the no-overwrite validation. The
    // mountpoint probe is mocked as successful so these tests stay focused on
    // discovery behavior.

    /*
     * Intent: verify that two present LUKS disks are both returned as
     *   candidates with zero accumulated skip notes.
     * Why: ensures the discovery phase correctly identifies all
     *   eligible disks on the happy path and does not synthesize
     *   spurious notes when every member is present and LUKS.
     * Scenario: normal 2-disk pool, both disks present and LUKS-formatted.
     */
    #[test]
    fn plan_discover_two_present_luks_disks() {
        let (req1, out1) = enroll_luks_uuid_ok("/dev/disk/by-id/d1", test_uuid(500).as_str());
        let (req2, out2) = enroll_luks_uuid_ok("/dev/disk/by-id/d2", test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let fs = enroll_fs(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = enroll_make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(plan.candidates[0].name.as_str(), "disk1");
        assert_eq!(plan.candidates[1].name.as_str(), "disk2");
        assert!(
            plan.notes.is_empty(),
            "plan.notes should be empty when all candidates present"
        );
    }

    // Intent: discovery rejects a member whose live LUKS UUID at the
    //   by-id path no longer matches the membership UUID key, before
    //   any slot mutation or slot inventory probe runs.
    // Why it exists: decision-024 mandates UUID re-checks at every
    //   mutation boundary; mount/replace/recover enforce this and enroll
    //   must too. Without it, a swapped or reformatted disk silently
    //   takes the operator's keyfile into slot 1 of a foreign LUKS
    //   container while the intended member's slot 1 stays empty,
    //   defeating auto-unlock at boot.
    // Scenario: operator's by-id stable path now points at a different
    //   LUKS volume than the one captured in pool.json (swap, reformat,
    //   or cloned disk). braid enroll fails before mutation, with the
    //   same wording shape as braid unlock.
    #[test]
    fn discover_rejects_luks_uuid_mismatch_before_slot_inventory() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let expected = test_uuid(500);
        let observed = "ffffffff-ffff-ffff-ffff-ffffffffffff";

        let (req1, out1) = enroll_luks_uuid_ok(d1, observed);
        let (req2, out2) = enroll_luks_uuid_ok(d2, test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2(d1)
            .with_luks_dump_text_luks2(d2)
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let (notes, result) = discover_enrollment_candidates(
            &runner,
            &fs,
            &membership,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );

        assert!(notes.is_empty(), "unexpected notes: {notes:?}");
        let err = result.expect_err("UUID mismatch must reject discovery");
        let msg = match err {
            EnrollKeyFileError::Validation(msg) => msg,
            other => panic!("expected validation error, got {other:?}"),
        };
        assert!(msg.contains("disk1"), "error should name disk1: {msg}");
        assert!(
            msg.contains("LUKS UUID mismatch"),
            "error should describe mismatch: {msg}"
        );
        assert!(
            msg.contains(expected.as_str()),
            "error should include expected UUID {expected}: {msg}"
        );
        assert!(
            msg.contains(observed),
            "error should include observed UUID {observed}: {msg}"
        );
        assert!(
            msg.contains("detach the foreign disk"),
            "error should include remediation guidance: {msg}"
        );
        assert!(
            msg.contains("braid replace"),
            "error should include replacement command: {msg}"
        );

        let requests = runner.requests();
        assert!(
            requests.iter().any(
                |r| matches!(r, CmdRequest::CryptsetupLuksDumpText { device } if device == d1)
            ),
            "gateway luksDump text probe for mismatched disk should run: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksDump { .. })),
            "slot inventory must not run after UUID mismatch: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
            "enrollment mutation must not run after UUID mismatch: {requests:?}"
        );
    }

    // Intent: preserved-context discovery failures return accumulated
    //   notes in DiskName order, not UUID order.
    // Why it exists: this function used to iterate membership.iter() and
    //   only sort notes after the loop completed, so early returns leaked
    //   UUID order to stderr.
    // Scenario: alpha is absent and zeta has a mismatched LUKS UUID.
    //   UUID order is zeta then alpha, but the one preserved note must
    //   be for alpha.
    #[test]
    fn preserved_context_failure_returns_notes_in_name_order() {
        let alpha_path = "/dev/disk/by-id/ata-A";
        let zeta_path = "/dev/disk/by-id/ata-Z";
        let mut membership = PoolMembership::empty();
        let (zeta_uuid, zeta) = disk_member(700, "zeta", zeta_path);
        let (alpha_uuid, alpha) = disk_member(701, "alpha", alpha_path);
        membership.insert(zeta_uuid, zeta).unwrap();
        membership.insert(alpha_uuid, alpha).unwrap();

        let observed = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let (req, out) = enroll_luks_uuid_ok(zeta_path, observed);
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2(zeta_path)
            .with_mapper_closed("braid-zeta");
        let fs = enroll_fs(&[zeta_path]);

        let (notes, result) = discover_enrollment_candidates(
            &runner,
            &fs,
            &membership,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );

        let err = result.expect_err("UUID mismatch must reject discovery");
        let msg = match err {
            EnrollKeyFileError::Validation(msg) => msg,
            other => panic!("expected validation error, got {other:?}"),
        };
        assert!(msg.contains("zeta"), "error should name zeta: {msg}");
        assert_eq!(notes.len(), 1, "expected one preserved note: {notes:?}");
        assert!(
            matches!(
                &notes[0],
                PreviewNote::PerDisk {
                    name,
                    level: NoteLevel::Skip,
                    message,
                } if name == "alpha" && message == "not present"
            ),
            "expected alpha skip note before UUID mismatch, got: {notes:?}"
        );
    }

    /*
     * Intent: an absent disk becomes a `PreviewNote::PerDisk { Skip }`
     *   on the successful plan, alongside the surviving LUKS candidate.
     * Why: the migration must replace today's `eprintln!("skip: X not
     *   present")` with a note surfaced via Preview/render_notes_for_stderr.
     *   Asserts both the candidate survives and the skip note shape/body
     *   are preserved.
     * Scenario: 2-disk pool but one disk is unplugged.
     */
    #[test]
    fn plan_discover_absent_disk_accumulates_skip_note() {
        let (req, out) = enroll_luks_uuid_ok("/dev/disk/by-id/d2", test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = enroll_fs(&["/dev/disk/by-id/d2"]);
        let membership = enroll_make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params)
            .expect("plan_enroll should succeed with one candidate");
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].name.as_str(), "disk2");
        assert_eq!(plan.notes.len(), 1);
        match &plan.notes[0] {
            PreviewNote::PerDisk {
                name,
                level,
                message,
            } => {
                assert_eq!(name, "disk1");
                assert!(matches!(level, NoteLevel::Skip));
                assert_eq!(message, "not present");
            }
            other => panic!("expected PerDisk Skip note, got {other:?}"),
        }
    }

    /*
     * Intent: a present-but-non-LUKS disk becomes a Skip note with
     *   body `not LUKS-formatted`, alongside the surviving LUKS candidate.
     * Why: distinguishes non-LUKS skip wording from absent-disk wording;
     *   drift here would silently change user-visible stderr text.
     * Scenario: config lists a disk that isn't LUKS-formatted yet.
     */
    #[test]
    fn plan_discover_non_luks_disk_accumulates_skip_note() {
        let (req1, out1) = enroll_luks_uuid_not_luks("/dev/disk/by-id/d1");
        let (req2, out2) = enroll_luks_uuid_ok("/dev/disk/by-id/d2", test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = enroll_fs(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = enroll_make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].name.as_str(), "disk2");
        assert_eq!(plan.notes.len(), 1);
        match &plan.notes[0] {
            PreviewNote::PerDisk {
                name,
                level,
                message,
            } => {
                assert_eq!(name, "disk1");
                assert!(matches!(level, NoteLevel::Skip));
                assert_eq!(message, "not LUKS-formatted");
            }
            other => panic!("expected PerDisk Skip note, got {other:?}"),
        }
    }

    /*
     * Intent: when every membership disk is absent, `plan_enroll`
     *   returns `Err(no present LUKS disks...)` with *all* accumulated
     *   skip notes preserved on `PlanFailure::notes` -- the preserved-context
     *   failure contract.
     * Why: this pins the shape A failure path for `enroll`. Today's
     *   behavior prints each `skip:` line before the validation error;
     *   the migrated code must surface the same ordered context so the
     *   cmd wrapper can render skips-then-error to stderr.
     * Scenario: all disks unplugged.
     */
    #[test]
    fn plan_all_absent_preserves_skip_notes_in_err() {
        let runner = MockRunner::default();
        let fs = enroll_fs(&[]);
        let membership = enroll_make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("expected no-candidates error"),
            Err(failure) => failure,
        };
        let err = &failure.error;
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
        );
        assert_eq!(
            failure.notes.len(),
            2,
            "both skip notes must survive the Err branch"
        );
        for (i, name) in ["disk1", "disk2"].iter().enumerate() {
            match &failure.notes[i] {
                PreviewNote::PerDisk {
                    name: actual_name,
                    level,
                    message,
                } => {
                    assert_eq!(actual_name, name);
                    assert!(matches!(level, NoteLevel::Skip));
                    assert_eq!(message, "not present");
                }
                other => panic!("expected PerDisk Skip, got {other:?}"),
            }
        }
    }

    /*
     * Intent: when every membership disk is present but non-LUKS,
     *   `plan_enroll` errors with preserved `not LUKS-formatted` skip
     *   notes.
     * Why: same preserved-context contract as all-absent, distinct
     *   skip-body path (non-LUKS vs. absent).
     * Scenario: disks are present but not yet LUKS-formatted.
     */
    #[test]
    fn plan_all_not_luks_preserves_skip_notes_in_err() {
        let (req, out) = enroll_luks_uuid_not_luks("/dev/disk/by-id/d1");
        let runner = MockRunner::default().with_output(req, out);
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("expected no-candidates error"),
            Err(failure) => failure,
        };
        let err = &failure.error;
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
        );
        assert_eq!(failure.notes.len(), 1);
        match &failure.notes[0] {
            PreviewNote::PerDisk {
                name,
                level,
                message,
            } => {
                assert_eq!(name, "disk1");
                assert!(matches!(level, NoteLevel::Skip));
                assert_eq!(message, "not LUKS-formatted");
            }
            other => panic!("expected PerDisk Skip, got {other:?}"),
        }
    }

    /*
     * Intent: the same accumulated Skip note renders bracketed in the
     *   dry-run `Preview` (stdout), and plain via
     *   `render_notes_for_stderr(..., Plain)` (real-run/failure stderr).
     * Why: the plan deliberately keeps enroll's real-run stderr wording
     *   as plain `skip: X not present` (byte-identical to today), while
     *   dry-run stdout uses the canonical bracketed shape. This pins
     *   both renderings to the single source-of-truth note on the plan.
     * Scenario: 1 absent + 1 present LUKS; plan.notes carries a single
     *   Skip note and we assert both stdout-shape and stderr-shape
     *   contain the disk1-skip line with their respective formats.
     */
    #[test]
    fn plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr() {
        let (req, out) = enroll_luks_uuid_ok("/dev/disk/by-id/d2", test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = enroll_fs(&["/dev/disk/by-id/d2"]);
        let membership = enroll_make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");

        let rendered_stdout = plan.preview().render();
        assert!(
            rendered_stdout.contains("[skip] disk disk1: not present\n"),
            "bracketed skip missing from preview stdout: {rendered_stdout}"
        );

        let rendered_stderr =
            preview::render_notes_for_stderr(&plan.notes, EnrollPlan::STDERR_STYLE);
        assert_eq!(rendered_stderr, "skip: disk1 not present\n");
    }

    // ---- plan_enroll dry-run probe tests ----

    /*
     * Intent: `--generate` rejects a missing target directory before
     *   probing pool disks.
     * Why it exists: generated key material must only be written under an
     *   existing mounted directory; a missing USB mount path must not fall
     *   through to LUKS discovery or passphrase work.
     * Scenario: user runs `braid enroll /mnt/usb --generate` before
     *   creating or mounting `/mnt/usb`.
     */
    #[test]
    fn generate_rejects_missing_directory_before_luks_discovery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().join("state"));
        let kf = tmp.path().join("missing").join("braid.key");
        let runner = MockRunner::default();
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("missing target directory must fail"),
            Err(failure) => failure,
        };
        let err = failure.error;

        assert!(
            err.to_string().contains("keyfile directory does not exist"),
            "unexpected error: {err}"
        );
        assert!(
            runner.requests().is_empty(),
            "missing directory must fail before any command runs; got {:?}",
            runner.requests()
        );
    }

    /*
     * Intent: `--generate` rejects a non-directory target before command
     *   execution.
     * Why it exists: a typo that points DIR at a regular file must not reach
     *   mountpoint checks, LUKS discovery, passphrase reads, or key creation.
     * Scenario: user passes a path whose parent component exists as a file.
     */
    #[test]
    fn generate_rejects_non_directory_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().join("state"));
        let not_dir = tmp.path().join("not-dir");
        std::fs::write(&not_dir, b"not a directory").unwrap();
        let kf = not_dir.join("braid.key");
        let runner = MockRunner::default();
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("non-directory target must fail"),
            Err(failure) => failure,
        };
        let err = failure.error;

        assert!(
            err.to_string()
                .contains("keyfile target is not a directory"),
            "unexpected error: {err}"
        );
        assert!(
            runner.requests().is_empty(),
            "non-directory target must fail before commands run; got {:?}",
            runner.requests()
        );
    }

    /*
     * Intent: `--generate` rejects an ordinary existing directory when
     *   `mountpoint -q` reports that it is not mounted.
     * Why it exists: this is the root-filesystem footgun the feature hardens:
     *   if the USB mount failed, braid must not create `DIR/braid.key` on the
     *   host filesystem.
     * Scenario: `/tmp/not-mounted` exists, but no USB or tmpfs is mounted
     *   there.
     */
    #[test]
    fn generate_rejects_plain_directory_before_luks_discovery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().join("state"));
        let target = tmp.path().join("not-mounted");
        std::fs::create_dir(&target).unwrap();
        let kf = target.join("braid.key");
        let runner = enroll_with_mountpoint_fail(MockRunner::default(), &target);
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("plain directory must fail"),
            Err(failure) => failure,
        };
        let err = failure.error;

        assert_eq!(
            err.to_string(),
            format!(
                "keyfile directory is not a mount point: {} -- mount the USB device there before running braid enroll --generate",
                target.display()
            )
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::MountpointCheck {
                path: MountPoint(target.display().to_string()),
            }],
            "mountpoint failure must stop before LUKS discovery"
        );
        assert!(!kf.exists(), "failed validation must not create braid.key");
    }

    /*
     * Intent: `--generate --dry-run` rejects an ordinary existing directory
     *   before producing a preview.
     * Why it exists: dry-run must enforce the same target safety gate as a
     *   real run and must not need LUKS/passphrase mocks to reject a bad
     *   generated-keyfile target.
     * Scenario: user previews key generation against a plain host directory
     *   where the USB mount did not happen.
     */
    #[test]
    fn generate_dry_run_rejects_plain_directory_without_key_creation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().join("state"));
        let target = tmp.path().join("not-mounted");
        std::fs::create_dir(&target).unwrap();
        let kf = target.join("braid.key");
        let runner = enroll_with_mountpoint_fail(MockRunner::default(), &target);
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: true,
                passphrase_stdin: false,
                passphrase_file: None,
                dry_run: true,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .expect_err("dry-run must reject a plain directory");

        assert!(
            err.to_string()
                .contains("keyfile directory is not a mount point"),
            "unexpected error: {err}"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::MountpointCheck {
                path: MountPoint(target.display().to_string()),
            }],
            "dry-run target validation must stop before LUKS discovery"
        );
        assert!(!kf.exists(), "dry-run failure must not create braid.key");
    }

    /*
     * Intent: `--generate` still refuses an existing `braid.key` when the
     *   target directory is mounted.
     * Why it exists: adding the mountpoint gate must not weaken the existing
     *   no-overwrite contract.
     * Scenario: USB is mounted at DIR, but DIR already contains braid.key.
     */
    #[test]
    fn generate_rejects_existing_keyfile_after_mountpoint_check() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().join("state"));
        let kf = tmp.path().join("braid.key");
        std::fs::write(&kf, b"existing").unwrap();
        let runner = enroll_with_mountpoint_ok(MockRunner::default(), tmp.path());
        let fs = enroll_fs(&["/dev/disk/by-id/d1"]);
        let membership = enroll_make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("existing generated keyfile must fail"),
            Err(failure) => failure,
        };
        let err = failure.error;

        assert!(
            err.to_string().contains("braid.key already exists"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("drop `--generate`"),
            "expected interrupted-run recovery hint in: {err}"
        );
        assert!(
            err.to_string()
                .contains(&format!("braid enroll {}", tmp.path().display())),
            "expected recovery command pointing at target dir in: {err}"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::MountpointCheck {
                path: MountPoint(tmp.path().display().to_string()),
            }],
            "existing-keyfile refusal must happen before LUKS discovery"
        );
    }

    /*
     * Intent: non-generate enroll does not require the keyfile directory to
     *   be a mount point.
     * Why it exists: only command paths that create `braid.key` need the
     *   mountpoint gate. Existing-keyfile consumers can read ordinary
     *   admin-controlled paths.
     * Scenario: user enrolls an existing keyfile from a temp directory.
     */
    #[test]
    fn non_generate_plan_does_not_require_mountpoint() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let (tmp, paths) = isolated_paths();
        let (kf, _) = enroll_make_existing_keyfile(&tmp);
        let runner = enroll_discovery_two_disks(d1, d2);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params)
            .expect("existing keyfile in ordinary directory should plan");

        assert_eq!(plan.candidates.len(), 2);
        assert!(
            runner
                .requests()
                .iter()
                .all(|request| !matches!(request, CmdRequest::MountpointCheck { .. })),
            "non-generate enroll must not call mountpoint -q; got {:?}",
            runner.requests()
        );
    }

    /*
     * Intent: direct existing-keyfile validation still accepts a regular file
     *   in an ordinary directory.
     * Why it exists: `add --enroll`, `replace --enroll`, and
     *   non-generate `enroll` share this helper and must not inherit the
     *   generate-only mountpoint requirement.
     * Scenario: `/run/keys/braid.key` or another admin-controlled regular
     *   file is used as an existing keyfile source.
     */
    #[test]
    fn validate_existing_keyfile_accepts_regular_file_without_mountpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let kf = dir.path().join("braid.key");
        std::fs::write(&kf, vec![0u8; KEYFILE_SIZE]).unwrap();

        validate_key_file_path(&kf, false).expect("existing regular keyfile should validate");
    }

    // Intent: direct existing-keyfile validation rejects short files.
    // Why it exists: `add --enroll`, `replace --enroll`, and non-generate
    //   `enroll` share this helper and must all enforce the 4096-byte contract.
    // Scenario: user points an existing-keyfile command at a small placeholder.
    #[test]
    fn validate_existing_keyfile_rejects_wrong_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let kf = dir.path().join("braid.key");
        std::fs::write(&kf, b"existing").unwrap();

        let err = validate_key_file_path(&kf, false).expect_err("short keyfile must fail");

        assert!(err.to_string().contains("4096"), "unexpected error: {err}");
    }

    /*
     * Intent: dry-run with one already-enrolled disk and one unenrolled
     *   disk emits a Skip note + zero steps for the enrolled disk and
     *   the enroll+backup step pair for the unenrolled one.
     * Why it exists: the core preview-fidelity fix. Before this change,
     *   the dry-run preview listed every candidate as `NeedsEnroll` even
     *   though the real run silently skipped already-enrolled disks via
     *   `plan_enrollment`'s `AlreadyEnrolled` branch. The assertion
     *   below pins both halves: the Skip note is present for disk1 and
     *   neither the `enroll keyfile` step nor the header-backup step
     *   appears for it.
     * Scenario: 2-disk pool, disk1 already has the keyfile in slot 1
     *   (e.g. partial earlier run), disk2 is freshly initialized.
     */
    #[test]
    fn dry_run_skips_already_enrolled_disks() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);

        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, &kf_str);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, &kf_str);
        let (slot2_req, slot2_out) = enroll_luks_dump_slot1_empty(d2);
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(slot2_req, slot2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");

        assert!(
            plan.notes.iter().any(|n| matches!(
                n,
                PreviewNote::PerDisk { name, level, message }
                    if name.as_str() == "disk1"
                        && matches!(level, NoteLevel::Skip)
                        && message == "keyfile already enrolled"
            )),
            "expected disk1 Skip note for already-enrolled keyfile, got notes: {:?}",
            plan.notes
        );
        assert!(
            plan.notes.iter().all(
                |n| !matches!(n, PreviewNote::PerDisk { name, .. } if name.as_str() == "disk2")
            ),
            "disk2 must not appear as a per-disk note; got: {:?}",
            plan.notes
        );

        let rendered = plan.preview().render();
        assert!(
            !rendered.contains("on /dev/disk/by-id/d1"),
            "preview must not list an enroll step for disk1 (already enrolled). Render: {rendered}"
        );
        assert!(
            rendered.contains("enroll keyfile -> LUKS slot 1 on /dev/disk/by-id/d2"),
            "preview must include enroll step for disk2. Render: {rendered}"
        );
        assert!(
            rendered.contains("LUKS header backup -> "),
            "preview must include header-backup step for disk2. Render: {rendered}"
        );
        assert!(
            rendered.contains("[skip] disk disk1: keyfile already enrolled\n"),
            "preview must include disk1 Skip note in bracketed style. Render: {rendered}"
        );
    }

    // Intent: dry-run plan_enroll routes the keyfile probe's
    //   wait/ok/skip rows through `emit_status` so they are visible
    //   to the test capture seam, in candidate order.
    // Why it exists: same row-emission contract as the real-run
    //   call-site test, but for the dry-run probe loop. A regression
    //   to raw `eprint!` would still pass the PreviewNote-shape
    //   `dry_run_skips_already_enrolled_disks` test while breaking
    //   the emit_status seam and the VM-test wording. This test
    //   owns the dry-run call-site row-emission contract.
    // Scenario: 2-disk pool, disk1 already has the keyfile in slot
    //   1, disk2 does not, and UUID order is opposite disk-name order;
    //   user runs `braid enroll --dry-run`.
    #[test]
    fn plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);

        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, &kf_str);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, &kf_str);
        let (uuid1_req, uuid1_out) = enroll_luks_uuid_ok(d1, test_uuid(501).as_str());
        let (uuid2_req, uuid2_out) = enroll_luks_uuid_ok(d2, test_uuid(500).as_str());
        let (slot2_req, slot2_out) = enroll_luks_dump_slot1_empty(d2);
        let runner = MockRunner::default()
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_luks_dump_text_luks2_for(&[d1, d2])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(slot2_req, slot2_out);
        let fs = enroll_fs(&[d1, d2]);
        let mut membership = PoolMembership::empty();
        let (u1, m1) = disk_member(501, "disk1", d1);
        let (u2, m2) = disk_member(500, "disk2", d2);
        membership.insert(u1, m1).unwrap();
        membership.insert(u2, m2).unwrap();

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");
        });

        let wait1 = "[wait] keyfile: checking against disk1...\n";
        let ok1 = "[ok]   keyfile: already enrolled on disk1\n";
        let wait2 = "[wait] keyfile: checking against disk2...\n";
        let skip2 = "[skip] keyfile: not yet enrolled on disk2\n";
        for line in [wait1, ok1, wait2, skip2] {
            assert!(
                captured.contains(line),
                "captured emit_status buffer missing {line:?}; got: {captured:?}"
            );
        }

        let pos = |needle: &str| {
            captured
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle:?} in {captured:?}"))
        };
        assert!(pos(wait1) < pos(ok1), "disk1 wait must precede ok");
        assert!(pos(ok1) < pos(wait2), "disk1 ok must precede disk2 wait");
        assert!(pos(wait2) < pos(skip2), "disk2 wait must precede skip");
    }

    // Intent: `--generate --dry-run` refuses a disk whose slot 1 is
    //   occupied by an unknown key before rendering a bogus enroll step.
    // Why it exists: generated-key dry-run used to bypass the slot-1
    //   inventory check, so preview succeeded even though the real run
    //   failed after prompting for the passphrase.
    // Scenario: operator runs `braid enroll /mnt/usb --generate
    //   --dry-run` against a 2-disk pool; an earlier troubleshooting
    //   session left an unknown key in disk2 slot 1 via `cryptsetup
    //   luksAddKey --key-slot 1`. `--generate` skips the keyfile probe
    //   and the slot-1 check refuses with the canonical recovery hint.
    #[test]
    fn dry_run_with_generate_surfaces_slot1_conflict() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (slot1_req, slot1_out) = enroll_luks_dump_slot1_empty(d1);
        let (slot2_req, slot2_out) = enroll_luks_dump_slot1_occupied(d2);
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(slot1_req, slot1_out)
            .with_output(slot2_req, slot2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("dry-run must fail on slot-1 conflict"),
            Err(failure) => failure,
        };
        let err = failure.error.to_string();

        assert!(
            err.contains("slot 1 on disk2"),
            "expected disk2 slot-1 error, got: {err}"
        );
        assert!(
            err.contains("occupied by an unknown key"),
            "expected canonical conflict wording, got: {err}"
        );
        let probe_count = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestKeyFile { .. }))
            .count();
        assert_eq!(
            probe_count, 0,
            "--generate dry-run must not probe the nonexistent keyfile"
        );
    }

    // Intent: existing-keyfile `--dry-run` refuses a rejected candidate
    //   whose slot 1 is occupied by an unknown key, while preserving
    //   earlier already-enrolled Skip notes on the failure.
    // Why it exists: existing-keyfile dry-run used to turn a Rejected
    //   keyfile probe directly into a preview step without running the
    //   real path's slot-1 conflict check.
    // Scenario: operator runs `braid enroll /mnt/usb --dry-run`
    //   against a 2-disk pool where disk1 already has the keyfile in
    //   slot 1 from a partial earlier run, and disk2 has a foreign key
    //   in slot 1. The keyfile probe authenticates on disk1 and is
    //   rejected on disk2; the slot-1 check refuses disk2 before the
    //   preview prints, with disk1's Skip note preserved.
    #[test]
    fn dry_run_existing_keyfile_surfaces_slot1_conflict() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);

        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, &kf_str);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, &kf_str);
        let (slot2_req, slot2_out) = enroll_luks_dump_slot1_occupied(d2);
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(slot2_req, slot2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("dry-run must fail on slot-1 conflict"),
            Err(failure) => failure,
        };
        let err = failure.error.to_string();

        assert!(
            err.contains("slot 1 on disk2"),
            "expected disk2 slot-1 error, got: {err}"
        );
        assert!(
            err.contains("occupied by an unknown key"),
            "expected canonical conflict wording, got: {err}"
        );
        assert!(
            failure.notes.iter().any(|n| matches!(
                n,
                PreviewNote::PerDisk { name, level, message }
                    if name.as_str() == "disk1"
                        && matches!(level, NoteLevel::Skip)
                        && message == "keyfile already enrolled"
            )),
            "disk1's Skip note must survive slot-1 conflict on disk2; got: {:?}",
            failure.notes
        );
    }

    /*
     * Intent: when every candidate is already enrolled, the dry-run
     *   preview emits zero step lines and surfaces the canonical
     *   `nothing to do.\n` footer alongside one Skip note per disk.
     * Why it exists: the idempotent re-enroll case is the headline
     *   user-visible benefit of the fix. Before this change, running
     *   `--dry-run` on a fully-enrolled pool listed both disks as
     *   `NeedsEnroll`, contradicting the actual no-op real run.
     * Scenario: 2-disk pool whose keyfile is already in slot 1 on both
     *   disks (e.g. user re-runs `braid enroll` after a successful
     *   first run).
     */
    #[test]
    fn dry_run_all_already_enrolled_emits_zero_steps() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);

        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, &kf_str);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_ok(d2, &kf_str);
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let plan = plan_enroll(&runner, &fs, &params).expect("plan_enroll should succeed");

        assert!(
            plan.steps.is_empty(),
            "no enroll steps when every disk is already enrolled, got: {:?}",
            plan.steps
        );

        let rendered = plan.preview().render();
        let expected = "[skip] disk disk1: keyfile already enrolled\n\
                        [skip] disk disk2: keyfile already enrolled\n\
                        nothing to do.\n";
        assert_eq!(
            rendered, expected,
            "full preview must equal exact byte-string"
        );
    }

    /*
     * Intent: dry-run with `--generate` skips the keyfile probe entirely
     *   and emits the same enroll+backup step set as today.
     * Why it exists: with `--generate`, the keyfile does not yet exist
     *   on disk, so probing it would always fail. Hoisting the dry-run
     *   probe without the `!generate` gate would make `--generate
     *   --dry-run` error with `Failed to open key file`. The mock
     *   omits any `CryptsetupTestKeyFile` response and the test asserts
     *   on `runner.requests()` so a regression that drops the gate
     *   surfaces both as the wrong assertion shape AND as a runtime
     *   `MissingMock` from MockRunner.
     * Scenario: user runs `braid enroll /mnt/usb --generate --dry-run`
     *   on a fresh USB stick before committing to the real run.
     */
    #[test]
    fn dry_run_with_generate_skips_probe() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (slot1_req, slot1_out) = enroll_luks_dump_slot1_empty(d1);
        let (slot2_req, slot2_out) = enroll_luks_dump_slot1_empty(d2);
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(slot1_req, slot1_out)
            .with_output(slot2_req, slot2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let _plan = plan_enroll(&runner, &fs, &params)
            .expect("plan_enroll should succeed in --generate dry-run mode");

        let probe_count = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestKeyFile { .. }))
            .count();
        assert_eq!(
            probe_count, 0,
            "--generate dry-run must not probe the (nonexistent) keyfile"
        );
    }

    /*
     * Intent: a non-Rejected probe error in the dry-run loop short-circuits
     *   `plan_enroll` with the error AND preserves any
     *   `keyfile already enrolled` Skip notes accumulated for earlier
     *   candidates that did probe successfully.
     * Why it exists: the dry-run probe is a `for` loop -- if a later
     *   candidate fails, naive error propagation would drop the
     *   already-pushed Skip notes from earlier iterations, giving
     *   users a confusing error context that hides the fact that
     *   disk1 was already enrolled. Without an explicit assertion on
     *   `PlanFailure::notes`, an implementation that returns
     *   `Err(PlanFailure { notes: Vec::new(), .. })` on
     *   probe error would silently pass.
     * Scenario: 2-disk pool, disk1 has the keyfile already, disk2's
     *   backing device is busy (stale dm-crypt mapper holding it
     *   open), so the probe exits 5.
     */
    #[test]
    fn dry_run_probe_error_propagates() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);

        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, &kf_str);
        let tkf2_busy_req = CmdRequest::CryptsetupTestKeyFile {
            device: d2.to_owned(),
            key_file_path: kf_str.clone(),
        };
        let tkf2_busy_out = enroll_err_raw(
            "cryptsetup open --test-passphrase --key-file",
            5,
            "Device /dev/dm-0 already exists.",
        );
        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_busy_req, tkf2_busy_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: None,
            dry_run: true,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let failure = match plan_enroll(&runner, &fs, &params) {
            Ok(_) => panic!("probe error must propagate as Err"),
            Err(failure) => failure,
        };
        let err = failure.error;

        match err {
            EnrollKeyFileError::Luks(LuksError::OpenFailed { exit_code, .. }) => {
                assert_eq!(exit_code, 5, "expected exit 5, got: {exit_code}");
            }
            other => panic!(
                "expected EnrollKeyFileError::Luks(LuksError::OpenFailed {{ exit_code: 5, .. }}), got: {other:?}"
            ),
        }

        assert!(
            failure.notes.iter().any(|n| matches!(
                n,
                PreviewNote::PerDisk { name, level, message }
                    if name.as_str() == "disk1"
                        && matches!(level, NoteLevel::Skip)
                        && message == "keyfile already enrolled"
            )),
            "disk1's accumulated Skip note must survive probe error on disk2; got: {:?}",
            failure.notes
        );
    }

    /*
     * Intent: real-run path (`dry_run = false`) does not probe the
     *   keyfile during planning. Verified end-to-end by invoking
     *   `cmd_enroll_key_file` with a wrong passphrase in
     *   `passphrase_file`, then asserting the error surfaces as
     *   wrong-passphrase AND that no `CryptsetupTestKeyFile` was
     *   recorded by MockRunner.
     * Why it exists: a regression that drops the `dry_run` gate would
     *   hoist the dry-run probe into every real-run `plan_enroll`,
     *   re-ordering operations and changing real-run stderr (probe
     *   errors before the passphrase prompt, double-emission of `ok:`
     *   lines). The two assertions defend independently: even if
     *   `runner.requests()` semantics ever changed, omitting the
     *   `CryptsetupTestKeyFile` mock means an accidental probe would
     *   surface as a `MissingMock` error rather than the intended
     *   wrong-passphrase validation error.
     * Scenario: user runs `braid enroll /tmp` with a passphrase file
     *   containing the wrong passphrase against a fresh 2-disk pool.
     */
    #[test]
    fn real_run_does_not_probe_before_passphrase() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let pass = "wrongpass";

        let (tmp, paths) = isolated_paths();
        let (kf, _kf_str) = enroll_make_existing_keyfile(&tmp);
        let pass_file = tmp.path().join("passphrase");
        std::fs::write(&pass_file, format!("{pass}\n")).unwrap();

        let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_fail(d1, pass);
        let runner = enroll_discovery_two_disks(d1, d2).with_output_stdin(tp_req, tp_stdin, tp_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: false,
            passphrase_stdin: false,
            passphrase_file: Some(pass_file.as_path()),
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };
        let err =
            cmd_enroll_key_file(&runner, &fs, &params).expect_err("wrong passphrase must surface");

        assert!(
            matches!(err, EnrollKeyFileError::Validation(ref msg) if msg.contains("wrong passphrase")),
            "expected wrong-passphrase validation error, got: {err:?}"
        );

        let probe_count = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestKeyFile { .. }))
            .count();
        assert_eq!(
            probe_count, 0,
            "real-run plan_enroll must not issue the keyfile probe before the passphrase verify"
        );
    }

    // ---- plan_enrollment tests ----

    /*
     * Intent: verify plan correctly identifies disks needing enrollment.
     * Why: normal first-time enrollment should classify all disks as NeedsEnroll.
     * Scenario: fresh pool, no keyfiles enrolled yet, slot 1 empty on all disks.
     */
    #[test]
    fn plan_all_need_enroll() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk1"),
                by_id: enroll_by_id(d1),
            }
        );
        assert_eq!(
            plan[1],
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk2"),
                by_id: enroll_by_id(d2),
            }
        );
    }

    /*
     * Intent: verify plan correctly identifies disks with keyfile already enrolled.
     * Why: re-enrollment should be idempotent -- no mutation needed.
     * Scenario: keyfile already in slot 1 on all disks.
     */
    #[test]
    fn plan_all_already_enrolled() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_ok(d2, kf);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(
            matches!(&plan[0], DiskEnrollAction::AlreadyEnrolled { name, .. } if name.as_str() == "disk1")
        );
        assert!(
            matches!(&plan[1], DiskEnrollAction::AlreadyEnrolled { name, .. } if name.as_str() == "disk2")
        );

        // Pin the "verify all candidates first, then probe" contract:
        // every passphrase verify happens before any keyfile probe.
        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupTestPassphrase {
                    device: d1.to_owned(),
                },
                CmdRequest::CryptsetupTestPassphrase {
                    device: d2.to_owned(),
                },
                CmdRequest::CryptsetupTestKeyFile {
                    device: d1.to_owned(),
                    key_file_path: kf.to_owned(),
                },
                CmdRequest::CryptsetupTestKeyFile {
                    device: d2.to_owned(),
                    key_file_path: kf.to_owned(),
                },
            ]
        );
    }

    /*
     * Intent: verify mixed scenarios are classified correctly.
     * Why: partial enrollment can occur if enrollment was interrupted.
     * Scenario: disk1 already enrolled, disk2 needs enrollment.
     */
    #[test]
    fn plan_mixed_enrolled_and_needs() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, kf);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(
            matches!(&plan[0], DiskEnrollAction::AlreadyEnrolled { name, .. } if name.as_str() == "disk1")
        );
        assert!(
            matches!(&plan[1], DiskEnrollAction::NeedsEnroll { name, .. } if name.as_str() == "disk2")
        );
    }

    // Intent: real-run plan_enrollment in ExistingKeyfile mode routes
    //   the keyfile probe's wait/ok/skip rows through `emit_status` so
    //   they are visible to the test capture seam, in candidate order.
    // Why it exists: the call site previously emitted these rows via
    //   raw `eprint!`, bypassing `emit_status` and silently regressing
    //   the codebase-wide convention. A regression to raw `eprint!`
    //   here would still pass the existing PreviewNote-shape tests but
    //   break VM-test wording and the emit_status seam. This test owns
    //   the row-emission contract for the real-run path.
    // Scenario: 2-disk pool, disk1 already has the keyfile in slot 1,
    //   disk2 does not.
    #[test]
    fn plan_enrollment_existing_keyfile_emits_keyfile_probe_rows_via_emit_status() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, kf);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            plan_enrollment(
                &runner,
                &candidates,
                Path::new(kf),
                &enroll_passphrase(pass),
                EnrollmentPlanMode::ExistingKeyfile,
            )
            .expect("plan_enrollment should succeed");
        });

        let wait1 = "[wait] keyfile: checking against disk1...\n";
        let ok1 = "[ok]   keyfile: already enrolled on disk1\n";
        let wait2 = "[wait] keyfile: checking against disk2...\n";
        let skip2 = "[skip] keyfile: not yet enrolled on disk2\n";
        for line in [wait1, ok1, wait2, skip2] {
            assert!(
                captured.contains(line),
                "captured emit_status buffer missing {line:?}; got: {captured:?}"
            );
        }

        let pos = |needle: &str| {
            captured
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle:?} in {captured:?}"))
        };
        assert!(pos(wait1) < pos(ok1), "disk1 wait must precede ok");
        assert!(pos(ok1) < pos(wait2), "disk1 ok must precede disk2 wait");
        assert!(pos(wait2) < pos(skip2), "disk2 wait must precede skip");
    }

    /*
     * Intent: verify wrong passphrase is detected early with the canonical
     *   per-disk error wording.
     * Why: wrong passphrase would cause all luksAddKey calls to fail -- catch
     *   it up front. Pinning the exact "wrong passphrase on {disk}" string
     *   prevents a regression that re-introduces the older
     *   "wrong passphrase (verified against ...)" form from the pre-batched
     *   verify split.
     * Scenario: user mistyped their passphrase.
     */
    #[test]
    fn plan_wrong_passphrase_errors() {
        let d1 = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let pass = "wrongpass";

        let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_fail(d1, pass);
        let runner = MockRunner::default().with_output_stdin(tp_req, tp_stdin, tp_out);

        let candidates = vec![enroll_candidate("disk1", d1)];

        let result = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "wrong passphrase on disk1");
    }

    /*
     * Intent: verify slot-1 conflict is detected during planning, not execution.
     * Why: THIS IS THE CORE REGRESSION this refactor fixes. The old code would
     *   have enrolled disk1 before discovering the conflict on disk2, leaving
     *   the pool in a partially-mutated state. The new code detects the conflict
     *   during planning before any mutation.
     * Scenario: disk1 slot 1 is empty (needs enroll), disk2 slot 1 has an
     *   unknown key (conflict). Plan must fail without producing a plan.
     */
    #[test]
    fn plan_slot1_conflict_errors() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_occupied(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let result = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("slot 1 on disk2"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("occupied by an unknown key"),
            "unexpected error: {err}"
        );
    }

    // Intent: `plan_single_disk_enrollment` returns one of three outcomes
    //   per candidate (AlreadyEnrolled / NeedsEnroll / Err on slot-1
    //   conflict) and emits the keyfile-probe wait/ok rows when in
    //   `ExistingKeyfile` mode and the keyfile authenticates.
    // Why it exists: this helper is the single entry point that
    //   `add` / `replace` / `enroll` all route through after this
    //   refactor. Pinning each branch directly (instead of only via
    //   `plan_enrollment`) prevents a regression where one caller's
    //   integration drifts and silently no-ops on a `Some(kf) +
    //   already-LUKS` target -- the bug this refactor fixes.
    // Scenario: an operator passes `--enroll DIR` to `replace` /
    //   `add` against a returning braid disk; the helper decides
    //   whether the disk needs a fresh `luksAddKey`, is idempotently
    //   skippable, or has slot 1 occupied by an unknown key.
    #[test]
    fn plan_single_disk_existing_keyfile_already_enrolled() {
        let d = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let (tkf_req, tkf_out) = enroll_test_keyfile_ok(d, kf);
        let runner = MockRunner::default().with_output(tkf_req, tkf_out);

        let action = plan_single_disk_enrollment(
            &runner,
            &disk("disk1"),
            &enroll_by_id(d),
            Path::new(kf),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .expect("Authenticated probe should yield AlreadyEnrolled");
        assert_eq!(
            action,
            DiskEnrollAction::AlreadyEnrolled {
                name: disk("disk1"),
                by_id: enroll_by_id(d),
            },
        );
    }

    #[test]
    fn plan_single_disk_existing_keyfile_needs_enroll() {
        let d = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let (tkf_req, tkf_out) = enroll_test_keyfile_fail(d, kf);
        let (ld_req, ld_out) = enroll_luks_dump_slot1_empty(d);
        let runner = MockRunner::default()
            .with_output(tkf_req, tkf_out)
            .with_output(ld_req, ld_out);

        let action = plan_single_disk_enrollment(
            &runner,
            &disk("disk1"),
            &enroll_by_id(d),
            Path::new(kf),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .expect("Rejected probe + empty slot 1 should yield NeedsEnroll");
        assert_eq!(
            action,
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk1"),
                by_id: enroll_by_id(d),
            },
        );
    }

    #[test]
    fn plan_single_disk_existing_keyfile_slot_one_occupied_errors() {
        let d = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let (tkf_req, tkf_out) = enroll_test_keyfile_fail(d, kf);
        let (ld_req, ld_out) = enroll_luks_dump_slot1_occupied(d);
        let runner = MockRunner::default()
            .with_output(tkf_req, tkf_out)
            .with_output(ld_req, ld_out);

        let err = plan_single_disk_enrollment(
            &runner,
            &disk("disk1"),
            &enroll_by_id(d),
            Path::new(kf),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .expect_err("slot-1-occupied must reject");
        let msg = err.to_string();
        assert!(msg.contains("slot 1 on disk1"), "unexpected error: {msg}");
        assert!(
            msg.contains("occupied by an unknown key"),
            "unexpected error: {msg}",
        );
    }

    /*
     * Intent: a non-auth exit from --test-passphrase --key-file (e.g.
     *   EBUSY) must surface as EnrollKeyFileError::Luks(OpenFailed) with
     *   exit_code 5, and MUST NOT silently fall through to the slot-1
     *   preflight as if the keyfile were "not yet enrolled".
     * Why it exists: this is the regression probe for the silent bug at
     *   the verify_key_file callsite. Before the VerifyOutcome refactor,
     *   `verify_key_file` returned `Ok(false)` for every non-zero exit,
     *   so an EBUSY here collapsed to "keyfile not enrolled -> proceed
     *   to luksDump and possibly enrollment" -- on a device that may not
     *   even be readable. No slot preflight mock is seeded below, so if
     *   the code regresses and reaches luksDump, MockRunner returns
     *   MissingMock and the error shape no longer matches OpenFailed{5}.
     * Scenario: a stale dm-crypt mapper holds the backing device busy,
     *   or another concurrent cryptsetup attempt is in flight, during a
     *   `braid enroll-key-file` run.
     */
    #[test]
    fn plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds() {
        let d1 = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_ok(d1, pass);

        // test-keyfile exits 5 (EBUSY) -- this is the regression signal.
        let tkf_req = CmdRequest::CryptsetupTestKeyFile {
            device: d1.to_owned(),
            key_file_path: kf.to_owned(),
        };
        let tkf_out = enroll_err_raw(
            "cryptsetup open --test-passphrase --key-file",
            5,
            "Device /dev/dm-0 already exists.",
        );

        // Deliberately NOT seeding CryptsetupLuksDump on d1. If the code
        // regresses and treats exit 5 as "not enrolled", it will proceed
        // to check_key_slot -> luksDump, which returns MissingMock and
        // changes the error shape -- the assertion below catches that.
        let runner = MockRunner::default()
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(tkf_req, tkf_out);

        let candidates = vec![enroll_candidate("disk1", d1)];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .expect_err("expected non-auth verify exit to surface as error");

        match err {
            EnrollKeyFileError::Luks(LuksError::OpenFailed {
                exit_code, hint, ..
            }) => {
                assert_eq!(exit_code, 5, "expected exit 5, got: {exit_code}");
                assert_eq!(hint, "device is already open or busy");
            }
            other => panic!(
                "expected EnrollKeyFileError::Luks(LuksError::OpenFailed {{ exit_code: 5, .. }}), got: {other:?}"
            ),
        }
    }

    /*
     * Intent: planner in `GenerateNew` mode never probes the (nonexistent)
     *   keyfile. Verify passphrase, then per-candidate slot-1 check only.
     * Why it exists: regression probe for the original `--generate` bug --
     *   the existing-keyfile planning path called `verify_key_file()` against
     *   a path that does not exist yet, aborting enrollment with
     *   "Failed to open key file" before the file could be created. This
     *   test deliberately omits any `CryptsetupTestKeyFile` mock; if the
     *   planner regresses and probes the keyfile, MockRunner returns
     *   MissingMock and the test fails before reaching the assertion.
     * Scenario: fresh USB stick, user runs `braid enroll /mnt/usb --generate`
     *   on a 2-disk pool with empty slot 1.
     */
    #[test]
    fn plan_generate_new_skips_keyfile_probe() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/mnt/usb/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);

        // Deliberately NO CryptsetupTestKeyFile mocks. If `GenerateNew`
        // mode regresses and calls `verify_key_file`, MockRunner returns
        // MissingMock and this test fails.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::GenerateNew,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk1"),
                by_id: enroll_by_id(d1),
            }
        );
        assert_eq!(
            plan[1],
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk2"),
                by_id: enroll_by_id(d2),
            }
        );
    }

    /*
     * Intent: in `GenerateNew` mode, each candidate's passphrase
     *   preflight runs exactly once.
     * Why it exists: `plan_enrollment` collects every candidate into a
     *   single batched `verify_credential_for_targets` call. A regression
     *   that double-counts a candidate (e.g. emits the same target into
     *   the slice twice, or re-verifies inside the loop) would still
     *   succeed because MockRunner mocks are reusable, so the duplicate
     *   call only shows up in the request log.
     * Scenario: 2-disk pool, new keyfile generation, both disks need
     *   enrollment and both slot-1 checks are empty.
     */
    #[test]
    fn plan_generate_new_does_not_repeat_first_candidate_passphrase_verify() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/mnt/usb/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::GenerateNew,
        )
        .expect("generate-new planning should succeed");

        assert_eq!(
            plan,
            vec![
                DiskEnrollAction::NeedsEnroll {
                    name: disk("disk1"),
                    by_id: enroll_by_id(d1),
                },
                DiskEnrollAction::NeedsEnroll {
                    name: disk("disk2"),
                    by_id: enroll_by_id(d2),
                },
            ]
        );
        for device in [d1, d2] {
            let count = runner
                .requests()
                .iter()
                .filter(|request| {
                    matches!(
                        request,
                        CmdRequest::CryptsetupTestPassphrase { device: requested }
                            if requested == device
                    )
                })
                .count();
            assert_eq!(
                count, 1,
                "expected exactly one passphrase verify for {device}"
            );
        }
    }

    /*
     * Intent: in `GenerateNew` mode, slot-1 conflict still aborts the plan
     *   without producing any actions.
     * Why it exists: skipping the keyfile probe must not weaken the slot-1
     *   conflict check -- otherwise `--generate` would create a useless
     *   keyfile, then fail mid-enrollment when `luksAddKey` collides with
     *   the existing slot.
     * Scenario: user runs `braid enroll DIR --generate` after a previous
     *   manual `cryptsetup luksAddKey --key-slot 1` left an unknown key
     *   in slot 1 on disk2.
     */
    #[test]
    fn plan_generate_new_slot1_conflict_errors() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/mnt/usb/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_occupied(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::GenerateNew,
        )
        .expect_err("expected slot-1 conflict to surface");
        assert!(
            err.to_string().contains("slot 1 on disk2"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("occupied by an unknown key"),
            "unexpected error: {err}"
        );
    }

    /*
     * Intent: in `ExistingKeyfile` mode, a divergent passphrase on disk2
     *   (the user ran `cryptsetup luksChangeKey` on disk2 out-of-band) is
     *   rejected during planning, before any disk is mutated or any
     *   keyfile probe runs.
     * Why it exists: the two-phase enroll refactor's stated guarantee is
     *   "no partial mutation on preflight failure". This holds for slot-1
     *   conflicts because `check_key_slot` runs per disk, and held for
     *   wrong-passphrase only against the first candidate. The batched
     *   up-front verify must include disk2 -- a regression that drops
     *   disk2 from the verify slice would let planning succeed and
     *   partial-mutate at apply time. The exact-equality assertion on
     *   the request log pins the new contract: only passphrase verifies
     *   run before the divergence is detected (no keyfile probes, no
     *   slot-1 dumps).
     * Scenario: 2-disk pool. Both disks have empty slot 1 (need enroll),
     *   but disk2's slot 0 holds a different passphrase from disk1 due
     *   to a previous out-of-band `cryptsetup luksChangeKey`.
     */
    #[test]
    fn plan_divergent_passphrase_existing_keyfile_errors_on_disk2() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_fail(d2, pass);

        // No keyfile-probe or luksDump mocks: the batched passphrase
        // verify rejects disk2 before any keyfile probe or slot-1 check
        // runs. A regression that drops disk2 from the verify slice
        // would reach those mocks and trip MissingMock.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .expect_err("expected divergent passphrase on disk2 to abort planning");

        let msg = err.to_string();
        assert!(
            msg.contains("wrong passphrase"),
            "expected 'wrong passphrase' in error: {msg}"
        );
        assert!(
            msg.contains("disk2"),
            "expected 'disk2' to be named in error: {msg}"
        );

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupTestPassphrase {
                    device: d1.to_owned(),
                },
                CmdRequest::CryptsetupTestPassphrase {
                    device: d2.to_owned(),
                },
            ]
        );
    }

    /*
     * Intent: in `GenerateNew` mode, a divergent passphrase on disk2 is
     *   rejected during planning, before the keyfile is generated or any
     *   disk is mutated.
     * Why it exists: same partial-mutation contract as the
     *   `ExistingKeyfile` divergent test, but exercising the
     *   `GenerateNew` code path (which skips the keyfile probe). The
     *   regression mode is the same: planner verifies passphrase only
     *   against disk1, divergent passphrase on disk2 surfaces at apply
     *   time. No `CryptsetupLuksDump` mock for d2 -- a regression to
     *   "verify only first candidate" trips MissingMock at d2's slot-1
     *   check.
     * Scenario: user runs `braid enroll DIR --generate` on a 2-disk pool
     *   where disk2's passphrase was changed out-of-band.
     */
    #[test]
    fn plan_divergent_passphrase_generate_new_errors_on_disk2() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/mnt/usb/braid.key";
        let pass = "testpass";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_fail(d2, pass);

        // No keyfile-probe mocks (GenerateNew skips that branch). No
        // `CryptsetupLuksDump` mock for d2 -- if the planner reaches
        // d2's slot-1 check, the per-disk passphrase verify regressed
        // and the test must fail loudly via MissingMock.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output(ld1_req, ld1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

        let candidates = vec![enroll_candidate("disk1", d1), enroll_candidate("disk2", d2)];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            &enroll_passphrase(pass),
            EnrollmentPlanMode::GenerateNew,
        )
        .expect_err("expected divergent passphrase on disk2 to abort planning");

        let msg = err.to_string();
        assert!(
            msg.contains("wrong passphrase"),
            "expected 'wrong passphrase' in error: {msg}"
        );
        assert!(
            msg.contains("disk2"),
            "expected 'disk2' to be named in error: {msg}"
        );
    }

    // ---- apply_enrollment tests ----

    /*
     * Intent: verify apply calls enroll_key_file for NeedsEnroll items.
     * Why: apply must translate the plan into actual cryptsetup mutations.
     * Scenario: plan with two NeedsEnroll items.
     */
    #[test]
    fn apply_enrolls_needs_enroll_items() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";
        let (_state_dir, paths) = isolated_paths();

        let (e1_req, e1_stdin, e1_out) = enroll_add_keyfile_ok(d1, kf, pass);
        let (e2_req, e2_stdin, e2_out) = enroll_add_keyfile_ok(d2, kf, pass);

        let runner = MockRunner::default()
            .with_output_stdin(e1_req, e1_stdin, e1_out)
            .with_output_stdin(e2_req, e2_stdin, e2_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d2.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk2.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            );

        let plan = vec![
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk1"),
                by_id: enroll_by_id(d1),
            },
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk2"),
                by_id: enroll_by_id(d2),
            },
        ];

        apply_enrollment(
            &runner,
            &plan,
            &enroll_passphrase(pass),
            Path::new(kf),
            &paths,
        )
        .unwrap();

        assert!(
            paths
                .luks_headers_dir()
                .join("braid-disk1.luksheader")
                .exists()
        );
        assert!(
            paths
                .luks_headers_dir()
                .join("braid-disk2.luksheader")
                .exists()
        );
    }

    /*
     * Intent: verify apply doesn't call enroll_key_file for AlreadyEnrolled.
     * Why: re-enrolling a key that's already present is wasteful and could fail.
     * Scenario: plan with only AlreadyEnrolled items — no mutations expected.
     */
    #[test]
    fn apply_skips_already_enrolled() {
        let d1 = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        // No mock outputs needed — if apply tried to call enroll_key_file,
        // MockRunner would return MissingMock error.
        let runner = MockRunner::default();

        let plan = vec![DiskEnrollAction::AlreadyEnrolled {
            name: disk("disk1"),
            by_id: enroll_by_id(d1),
        }];

        let (_state_dir, paths) = isolated_paths();
        apply_enrollment(
            &runner,
            &plan,
            &enroll_passphrase(pass),
            Path::new(kf),
            &paths,
        )
        .unwrap();
    }

    /*
     * Intent: verify apply handles mixed plans correctly.
     * Why: only NeedsEnroll items should trigger mutations.
     * Scenario: one disk already enrolled, one needs enrollment.
     */
    #[test]
    fn apply_mixed_plan() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";
        let (_state_dir, paths) = isolated_paths();

        // Only d2 should have enroll called — d1 is AlreadyEnrolled
        let (e2_req, e2_stdin, e2_out) = enroll_add_keyfile_ok(d2, kf, pass);
        let runner = MockRunner::default()
            .with_output_stdin(e2_req, e2_stdin, e2_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d2.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk2.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            );

        let plan = vec![
            DiskEnrollAction::AlreadyEnrolled {
                name: disk("disk1"),
                by_id: enroll_by_id(d1),
            },
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk2"),
                by_id: enroll_by_id(d2),
            },
        ];

        apply_enrollment(
            &runner,
            &plan,
            &enroll_passphrase(pass),
            Path::new(kf),
            &paths,
        )
        .unwrap();

        assert!(
            !paths
                .luks_headers_dir()
                .join("braid-disk1.luksheader")
                .exists()
        );
        assert!(
            paths
                .luks_headers_dir()
                .join("braid-disk2.luksheader")
                .exists()
        );
    }

    // Intent: apply_enrollment enriches a local LUKS header-backup failure
    // after keyfile enrollment has already succeeded.
    // Why it exists: rerunning enroll after this point should not imply the
    // slot mutation failed; the user needs the direct off-system backup path.
    // Scenario: keyfile enrollment succeeds for one disk, then the state
    // directory cannot accept the local header backup.
    #[test]
    fn apply_enrollment_returns_enriched_error_when_backup_fails() {
        let d1 = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let pass = "testpass";
        let (_state_dir, paths) = isolated_paths();

        let (enroll_req, enroll_stdin, enroll_out) = enroll_add_keyfile_ok(d1, kf, pass);
        let runner = MockRunner::default()
            .with_output_stdin(enroll_req, enroll_stdin, enroll_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                enroll_err_raw("cryptsetup luksHeaderBackup", 1, "No space left on device"),
            );
        let plan = vec![DiskEnrollAction::NeedsEnroll {
            name: disk("disk1"),
            by_id: enroll_by_id(d1),
        }];

        let err = apply_enrollment(
            &runner,
            &plan,
            &enroll_passphrase(pass),
            Path::new(kf),
            &paths,
        )
        .expect_err("backup failure should abort enrollment apply")
        .to_string();

        assert!(
            err.contains("cryptsetup luksHeaderBackup --header-backup-file"),
            "expected remediation command in: {err}"
        );
        assert!(err.contains(d1), "expected disk by-id path in: {err}");
        assert!(
            err.contains("after the LUKS mutation completed"),
            "expected post-mutation framing in: {err}"
        );
    }

    // Intent: apply_enrollment aborts at the first disk's post-mutation
    // backup failure and leaves every later disk fully unmutated.
    // Why it exists: decision-019's recoverability argument for skipping the
    // sleep inhibitor depends on later disks staying untouched for the
    // idempotent re-run; the single-disk backup-failure test cannot catch a
    // reorder or swallowed error that mutates disk2.
    // Scenario: enroll runs over two disks, disk1's keyfile enrollment
    // succeeds, the local .luksheader backup fails because the state dir is
    // full, and disk2 is left untouched for the retry.
    #[test]
    fn apply_enrollment_backup_failure_leaves_later_disk_unmutated() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";
        let (_state_dir, paths) = isolated_paths();

        let (e1_req, e1_stdin, e1_out) = enroll_add_keyfile_ok(d1, kf, pass);
        let (e2_req, e2_stdin, e2_out) = enroll_add_keyfile_ok(d2, kf, pass);
        let runner = MockRunner::default()
            .with_output_stdin(e1_req, e1_stdin, e1_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                enroll_err_raw("cryptsetup luksHeaderBackup", 1, "No space left on device"),
            )
            .with_output_stdin(e2_req, e2_stdin, e2_out);
        let plan = vec![
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk1"),
                by_id: enroll_by_id(d1),
            },
            DiskEnrollAction::NeedsEnroll {
                name: disk("disk2"),
                by_id: enroll_by_id(d2),
            },
        ];

        let err = apply_enrollment(
            &runner,
            &plan,
            &enroll_passphrase(pass),
            Path::new(kf),
            &paths,
        )
        .expect_err("disk1 backup failure should abort enrollment apply")
        .to_string();

        assert!(
            err.contains("cryptsetup luksHeaderBackup --header-backup-file"),
            "expected remediation command in: {err}"
        );
        assert!(err.contains(d1), "expected disk by-id path in: {err}");
        assert!(
            err.contains("after the LUKS mutation completed"),
            "expected post-mutation framing in: {err}"
        );

        let requests = runner.requests();
        let add_pos = requests
            .iter()
            .position(
                |r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { device, .. } if device == d1),
            )
            .expect("disk1 keyfile enrollment must run before backup failure");
        let backup_pos = requests
            .iter()
            .position(
                |r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { device, .. } if device == d1),
            )
            .expect("disk1 header backup must be attempted and fail");
        assert!(
            add_pos < backup_pos,
            "post-mutation backup ordering requires enroll before backup; \
             add_pos={add_pos}, backup_pos={backup_pos}, requests={requests:?}"
        );
        assert!(
            !requests.iter().any(
                |r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { device, .. } if device == d2)
            ),
            "disk2 keyfile enrollment must be left for the idempotent re-run; requests={requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { device, .. } if device == d2)),
            "disk2 header backup must be left for the idempotent re-run; requests={requests:?}"
        );
    }

    // Intent: EnrollPlan::execute enriches the apply-phase error with the
    // drop-`--generate` recovery hint when --generate succeeded.
    // Why it exists: a partial luksAddKey failure on disk2 leaves an orphan
    // keyfile on the USB; without the hint, the documented retry command
    // points users toward generating a new, mismatched keyfile.
    // Scenario: 2-disk plan from plan_enrollment, disk1 enroll + backup ok,
    // disk2 luksAddKey returns nonzero, and the user sees the reuse-keyfile
    // retry command.
    #[test]
    fn execute_generate_partial_failure_reports_recovery_hint() {
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let kf_str = kf.display().to_string();
        let pass = "testpass";
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, format!("{pass}\n")).unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        // This plan is built directly (no plan_enroll discovery), so the
        // execute-time re-probe needs its own CryptsetupLuksUuid mock per
        // by-id returning the carried candidate uuid -- otherwise the
        // re-probe MissingMocks before reaching the luksAddKey apply path.
        let (uuid1_req, uuid1_out) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
        let (uuid2_req, uuid2_out) = enroll_luks_uuid_ok(d2, test_uuid(501).as_str());
        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);
        let (e1_req, e1_stdin, e1_out) = enroll_add_keyfile_ok(d1, &kf_str, pass);
        let (e2_req, e2_stdin, e2_out) = enroll_add_keyfile_fail(d2, &kf_str, pass);
        let (mp_req, mp_out) = enroll_mountpoint_ok(tmp.path());

        let runner = MockRunner::default()
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out)
            .with_output(mp_req, mp_out)
            .with_output_stdin(e1_req, e1_stdin, e1_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            )
            .with_output_stdin(e2_req, e2_stdin, e2_out);

        let plan = EnrollPlan {
            notes: vec![],
            steps: vec![],
            candidates: vec![
                EnrollmentCandidate {
                    name: disk("disk1"),
                    by_id: enroll_by_id(d1),
                    uuid: test_uuid(500),
                },
                EnrollmentCandidate {
                    name: disk("disk2"),
                    by_id: enroll_by_id(d2),
                    uuid: test_uuid(501),
                },
            ],
            generate: true,
        };
        let membership = enroll_make_membership(&[]);
        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: Some(&pass_path),
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let err = plan
            .execute(&runner, &params)
            .expect_err("disk2 luksAddKey failure should abort generate execute")
            .to_string();

        assert!(kf.exists(), "generate execute should create braid.key");
        assert!(
            err.contains("cryptsetup luksAddKey failed"),
            "expected underlying luksAddKey failure in: {err}"
        );
        assert!(
            err.contains("drop `--generate`"),
            "expected drop-generate recovery hint in: {err}"
        );
        assert!(
            err.contains(&format!("braid enroll {}", tmp.path().display())),
            "expected recovery command pointing at generated-key dir in: {err}"
        );
    }

    // ---- reprobe_member_luks_uuid tests ----

    // Intent: reprobe_member_luks_uuid rejects a candidate whose live LUKS
    //   UUID at the by-id path no longer matches the UUID discovery
    //   validated, returning the canonical mismatch message.
    // Why it exists: decision-024 mandates a UUID re-check at every
    //   mutation boundary. The enroll passphrase prompt is an
    //   operator-controlled window in which a disk can be swapped to a
    //   foreign LUKS container that shares the pool passphrase; this pins
    //   the guard's reject arm. No VM test -- an in-window physical swap
    //   during the passphrase prompt is not deterministically reproducible
    //   in a NixOS VM (tests/cli/enroll-uuid-mismatch.py covers the
    //   pre-command discovery case).
    // Scenario: the by-id slot now points at a different LUKS volume than
    //   the one discovery captured; the re-probe reads the foreign UUID.
    #[test]
    fn reprobe_member_luks_uuid_mismatch_rejects() {
        let d1 = "/dev/disk/by-id/d1";
        let expected = test_uuid(500);
        let observed = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let (req, out) = enroll_luks_uuid_ok(d1, observed);
        let runner = MockRunner::default().with_output(req, out);

        let err = reprobe_member_luks_uuid(&runner, &disk("disk1"), &enroll_by_id(d1), &expected)
            .expect_err("a foreign live UUID must reject the re-probe");
        let msg = err.to_string();
        assert!(
            msg.contains("LUKS UUID mismatch"),
            "expected mismatch wording: {msg}"
        );
        assert!(msg.contains("disk1"), "error should name disk1: {msg}");
        assert!(
            msg.contains(expected.as_str()),
            "error should include expected UUID {expected}: {msg}"
        );
        assert!(
            msg.contains(observed),
            "error should include observed UUID {observed}: {msg}"
        );
        assert!(
            msg.contains("detach the foreign disk"),
            "error should include remediation guidance: {msg}"
        );

        let probe_count = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupLuksUuid { device } if device == d1))
            .count();
        assert_eq!(probe_count, 1, "exactly one luksUUID probe must run");
    }

    // Intent: reprobe_member_luks_uuid fails closed when the live UUID
    //   cannot be confirmed (device absent, non-LUKS, or otherwise
    //   unreadable at probe time), and issues no enrollment mutation.
    // Why it exists: a mid-prompt disappearance is indistinguishable from
    //   a swap in progress, so the re-probe must hard-fail rather than
    //   proceed -- the fail-closed arm of the decision-024 guard. Same
    //   no-VM-test rationale as the mismatch sibling.
    // Scenario: the disk was disconnected or reformatted to a non-LUKS
    //   device during the passphrase prompt, so luksUUID exits non-zero.
    #[test]
    fn reprobe_member_luks_uuid_probe_failure_fails_closed() {
        let d1 = "/dev/disk/by-id/d1";
        let (req, out) = enroll_luks_uuid_not_luks(d1);
        let runner = MockRunner::default().with_output(req, out);

        let err =
            reprobe_member_luks_uuid(&runner, &disk("disk1"), &enroll_by_id(d1), &test_uuid(500))
                .expect_err("an unconfirmable UUID must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot confirm LUKS UUID"),
            "expected fail-closed wording: {msg}"
        );
        assert!(
            msg.contains("re-run `braid enroll`"),
            "expected re-run guidance: {msg}"
        );

        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
            "no enrollment mutation may follow a fail-closed re-probe: {:?}",
            runner.requests()
        );
    }

    // Intent: the discovery->execute window is closed -- a disk swapped to
    //   a foreign LUKS container during the passphrase prompt is rejected
    //   at the execute-time re-probe, before any keyfile is written to
    //   slot 1 or generated on the USB.
    // Why it exists: discovery validates each member's UUID, but the
    //   passphrase prompt that follows is an operator-controlled window.
    //   Without the execute-boundary re-probe (decision-024), a swap in
    //   that window silently lands the keyfile in the wrong container
    //   while the intended member's slot 1 stays empty. No VM test -- an
    //   in-window physical swap during the passphrase prompt is not
    //   deterministically reproducible in a NixOS VM;
    //   tests/cli/enroll-uuid-mismatch.py covers the pre-command reformat
    //   (discovery) case.
    // Scenario: 2-disk pool, --generate. disk1's by-id slot still matches
    //   at discovery but is swapped to a foreign LUKS volume before
    //   execute re-probes it.
    #[test]
    fn execute_rejects_swapped_disk_before_mutation() {
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, "testpass\n").unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let foreign = "ffffffff-ffff-ffff-ffff-ffffffffffff";

        // Mappers closed => discovery issues exactly one luksUUID per disk,
        // so the 2nd sequence element below is consumed by the execute
        // re-probe. A mapper-open disk would pop both at discovery and
        // silently invert the test -- the mismatch would surface at plan
        // time rather than at the execute boundary under test.
        let (_, d1_match) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
        let (_, d1_swapped) = enroll_luks_uuid_ok(d1, foreign);
        let (d2_req, d2_out) = enroll_luks_uuid_ok(d2, test_uuid(501).as_str());
        let runner = MockRunner::default()
            .with_output_sequence(
                CmdRequest::CryptsetupLuksUuid {
                    device: d1.to_owned(),
                },
                vec![d1_match, d1_swapped],
            )
            .with_output(d2_req, d2_out)
            .with_luks_dump_text_luks2_for(&[d1, d2])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let params = EnrollKeyFileParams {
            membership: &membership,
            key_file_path: &kf,
            generate: true,
            passphrase_stdin: false,
            passphrase_file: Some(&pass_path),
            dry_run: false,
            paths: &paths,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        };

        let plan =
            plan_enroll(&runner, &fs, &params).expect("discovery must succeed with matching UUIDs");
        let err = plan
            .execute(&runner, &params)
            .expect_err("execute re-probe must reject the swapped disk")
            .to_string();

        assert!(
            err.contains("LUKS UUID mismatch"),
            "expected mismatch error: {err}"
        );
        assert!(
            err.contains("disk1"),
            "error should name the swapped disk: {err}"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
            "no keyfile may be enrolled after a swap is detected: {:?}",
            runner.requests()
        );
        assert!(
            !kf.exists(),
            "--generate must not create braid.key when the re-probe rejects"
        );
    }

    // ---- generate_key_file tests ----

    /*
     * Intent: verify --generate rejects existing keyfile.
     * Why: --generate must not silently overwrite an existing keyfile.
     * Scenario: user runs --generate when the target file already exists on USB.
     */
    #[test]
    fn generate_rejects_existing_keyfile() {
        let dir = tempfile::tempdir().unwrap();
        let kf = dir.path().join("braid.key");
        std::fs::write(&kf, b"existing").unwrap();

        let err = super::generate_key_file(&kf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    /*
     * Intent: verify generate_key_file creates a 4096-byte file with mode 0o400.
     * Why: the keyfile must be exactly 4096 bytes of random data with restrictive
     *   permissions to match cryptsetup --keyfile-size expectations.
     * Scenario: normal --generate on a writable directory.
     */
    #[test]
    fn generate_key_file_creates_4096_bytes_mode_400() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let kf = dir.path().join("braid.key");

        super::generate_key_file(&kf).unwrap();

        let meta = std::fs::metadata(&kf).unwrap();
        assert_eq!(meta.len(), 4096);
        assert_eq!(meta.permissions().mode() & 0o777, 0o400);
    }

    /*
     * Intent: verify create_new(true) prevents TOCTOU race.
     * Why: if a file appears between the existence check and generation,
     *   create_new(true) ensures we fail rather than overwrite.
     * Scenario: concurrent process creates braid.key after our check.
     */
    #[test]
    fn generate_key_file_create_new_rejects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let kf = dir.path().join("braid.key");
        // Simulate a file appearing between check and create
        std::fs::write(&kf, b"raced").unwrap();

        let err = super::generate_key_file(&kf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    /*
     * Intent: enroll is blocked when a pending-operation journal exists.
     * Why: enroll reads pool.json membership to discover disks — if membership
     *   is inconsistent (mid-recovery), it could miss disks or target stale ones.
     * Scenario: an add was interrupted; pending-op.json exists. User runs
     *   braid enroll before braid recover.
     */
    #[test]
    fn cmd_enroll_blocked_in_recovery_mode() {
        let (tmp, paths) = isolated_paths();

        // Create a pending-op journal
        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                phase: crate::journal::AddPhase::PoolMutation,
                targets: crate::membership::LuksUuidMap::new(),
            },
        );
        crate::journal::write_journal(&paths, &journal).unwrap();

        // No mock commands — if enroll reaches cryptsetup, MockRunner will panic
        let runner = MockRunner::default();
        let fs = enroll_fs(&[]);
        let membership = enroll_make_membership(&[("d1", "/dev/disk/by-id/d1")]);
        let kf = tmp.path().join("braid.key");

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: false,
                passphrase_stdin: false,
                passphrase_file: None,
                dry_run: false,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("interrupted operation"),
            "expected 'interrupted operation' in: {err}"
        );
    }

    // Intent: dry-run enroll is also blocked when a pending-operation
    //   journal exists, just like a real run.
    // Why it exists: dry-run is a faithful preview of the real run.
    //   A regression that gated `check_no_pending_operation` behind
    //   `if !dry_run` would let `braid enroll DIR --dry-run` produce a
    //   preview against possibly-stale pool.json membership that the
    //   real run would refuse -- violating the dry-run-as-faithful-preview
    //   invariant. The sibling test
    //   `cmd_enroll_blocked_in_recovery_mode` pins the real-run path;
    //   this one pins the dry-run path.
    // Scenario: an add was interrupted; pending-op.json exists. User
    //   runs `braid enroll --dry-run` to see what would happen before
    //   deciding whether to recover.
    #[test]
    fn cmd_enroll_dry_run_blocked_in_recovery_mode() {
        let (tmp, paths) = isolated_paths();

        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                phase: crate::journal::AddPhase::PoolMutation,
                targets: crate::membership::LuksUuidMap::new(),
            },
        );
        crate::journal::write_journal(&paths, &journal).unwrap();

        let runner = MockRunner::default();
        let fs = enroll_fs(&[]);
        let membership = enroll_make_membership(&[("d1", "/dev/disk/by-id/d1")]);
        let kf = tmp.path().join("braid.key");

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: false,
                passphrase_stdin: false,
                passphrase_file: None,
                dry_run: true,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("interrupted operation"),
            "expected 'interrupted operation' in: {err}"
        );
    }

    // Intent: standalone `braid enroll` never writes the pending-op
    //   journal, so an interrupted apply phase cannot strand the user
    //   in recovery mode.
    // Why it exists: this property is the justification recorded in
    //   `docs/design/decisions/019-inhibit-sleep.md`'s "Excluded: braid
    //   enroll" subsection for not acquiring a sleep inhibitor; a
    //   regression that silently introduces a journal write to enroll
    //   would invalidate that justification without failing any
    //   existing test.
    // Scenario: an operator runs `braid enroll`, the second disk's
    //   cryptsetup invocation fails mid-loop, and `pending-op.json`
    //   must still not exist afterward so the operator is not pushed
    //   into recovery mode for a non-recovery error.
    #[test]
    fn cmd_enroll_apply_failure_does_not_write_pending_op_journal() {
        let (tmp, paths) = isolated_paths();
        let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);
        let pass = "testpass";
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, format!("{pass}\n")).unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";

        let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = enroll_test_keyfile_fail(d1, &kf_str);
        let (tkf2_req, tkf2_out) = enroll_test_keyfile_fail(d2, &kf_str);
        let (ld1_req, ld1_out) = enroll_luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = enroll_luks_dump_slot1_empty(d2);
        let (e1_req, e1_stdin, e1_out) = enroll_add_keyfile_ok(d1, &kf_str, pass);
        let (e2_req, e2_stdin, e2_out) = enroll_add_keyfile_fail(d2, &kf_str, pass);

        let runner = enroll_discovery_two_disks(d1, d2)
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out)
            .with_output_stdin(e1_req, e1_stdin, e1_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            )
            .with_output_stdin(e2_req, e2_stdin, e2_out);
        let fs = enroll_fs(&[d1, d2]);
        let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: false,
                passphrase_stdin: false,
                passphrase_file: Some(&pass_path),
                dry_run: false,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .expect_err("disk2 luksAddKey failure should abort enroll")
        .to_string();

        assert!(
            err.contains("cryptsetup luksAddKey failed"),
            "expected disk2 luksAddKey failure in: {err}"
        );
        assert!(
            !paths.pending_op_json().exists(),
            "standalone enroll must not create pending-op.json after an apply failure"
        );
    }

    // Intent: --generate refuses to write braid.key if DIR was a mount
    //   point at plan time but no longer is when execute reaches the
    //   write -- and the re-check fires after the slow planning window
    //   (passphrase verify + slot inventory), not at the top of execute.
    // Why it exists: pins the TOCTOU fix at the execute-time re-check.
    //   The plan-time mountpoint gate alone cannot prevent a key leak
    //   onto the host root filesystem if DIR is unmounted between the
    //   early check and OpenOptions::create_new. The window includes
    //   passphrase read, Argon2 --test-passphrase verify, and per-disk
    //   luksDump. A re-check positioned at the top of execute (before
    //   plan_enrollment) would silently leave the same window open; the
    //   ordering assertion below pins that the re-check is placed
    //   immediately before generate_key_file. See docs/internals/luks-unlock.md
    //   "Keyfile creation target invariant".
    // Scenario: operator mounted /tmp/usb correctly, ran enroll, but
    //   systemd-automount timed out (or admin manually unmounted)
    //   between passphrase prompt and key generation.
    #[test]
    fn cmd_generate_mountpoint_revoked_between_plan_and_write() {
        let (tmp, paths) = isolated_paths();
        let kf = tmp.path().join("braid.key");
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, "rightpass\n").unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let (uuid_req, uuid_out) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
        let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_ok(d1, "rightpass");
        let (slot_req, slot_out) = enroll_luks_dump_slot1_empty(d1);

        let (mp_req, mp_ok_out) = enroll_mountpoint_ok(tmp.path());
        let (_, mp_fail_out) = enroll_mountpoint_fail(tmp.path());

        let runner = MockRunner::default()
            .with_output_sequence(mp_req, vec![mp_ok_out, mp_fail_out])
            .with_output(uuid_req, uuid_out)
            .with_luks_dump_text_luks2(d1)
            .with_mappers_closed(&["braid-disk1"])
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(slot_req, slot_out);

        let fs = enroll_fs(&[d1]);
        let membership = enroll_make_membership(&[("disk1", d1)]);

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: true,
                passphrase_stdin: false,
                passphrase_file: Some(&pass_path),
                dry_run: false,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .expect_err("re-check must fail when mountpoint disappears mid-run");

        assert!(
            err.to_string()
                .contains("was a mount point at plan time but is no longer mounted"),
            "expected execute-time mountpoint error, got: {err}"
        );
        assert!(
            !kf.exists(),
            "braid.key must not be created when re-check fails"
        );

        let reqs = runner.requests();

        assert!(
            !reqs
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
            "no luksAddKey calls expected; got requests: {reqs:?}"
        );
        assert!(
            !reqs
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. })),
            "no header backup expected; got requests: {reqs:?}"
        );

        let mp_positions: Vec<usize> = reqs
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, CmdRequest::MountpointCheck { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            mp_positions.len(),
            2,
            "expected two MountpointCheck calls (plan + execute), got {} in {reqs:?}",
            mp_positions.len()
        );

        let tp_position = reqs
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { .. }))
            .expect("CryptsetupTestPassphrase must run as part of verify");
        let dump_position = reqs
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksDump { .. }))
            .expect("CryptsetupLuksDump (slot inventory) must run as part of plan_enrollment");
        assert!(
            mp_positions[0] < tp_position,
            "plan-time mountpoint check must precede passphrase verify; got mp_positions={mp_positions:?} tp={tp_position} in {reqs:?}"
        );
        assert!(
            mp_positions[1] > tp_position,
            "execute-time mountpoint re-check must follow passphrase verify (not be at top of execute); got mp_positions={mp_positions:?} tp={tp_position} in {reqs:?}"
        );
        assert!(
            mp_positions[1] > dump_position,
            "execute-time mountpoint re-check must follow slot inventory; got mp_positions={mp_positions:?} dump={dump_position} in {reqs:?}"
        );
    }

    /*
     * Intent: `cmd_enroll_key_file` with `generate=true` and a wrong
     *   passphrase fails with the standard wrong-passphrase validation,
     *   AND no `braid.key` is created on disk.
     * Why it exists: --generate must atomically validate first, generate
     *   the keyfile only on success. A user who fat-fingers their
     *   passphrase should not be left with a useless 4096-byte key file
     *   sitting on their USB stick that they then have to identify and
     *   remove manually before retrying.
     * Scenario: user runs `braid enroll /mnt/usb --generate --passphrase-file FILE`
     *   with the wrong passphrase in FILE.
     */
    #[test]
    fn cmd_generate_wrong_passphrase_no_keyfile_created() {
        let (tmp, paths) = isolated_paths();

        let kf = tmp.path().join("braid.key");
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, "wrongpass\n").unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let (uuid_req, uuid_out) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
        let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_fail(d1, "wrongpass");

        let runner = MockRunner::default()
            .with_output(uuid_req, uuid_out)
            .with_luks_dump_text_luks2(d1)
            .with_mappers_closed(&["braid-disk1"])
            .with_output_stdin(tp_req, tp_stdin, tp_out);
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let fs = enroll_fs(&[d1]);
        let membership = enroll_make_membership(&[("disk1", d1)]);

        let err = cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: true,
                passphrase_stdin: false,
                passphrase_file: Some(&pass_path),
                dry_run: false,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .expect_err("expected wrong passphrase to abort enrollment");

        assert!(
            err.to_string().contains("wrong passphrase"),
            "unexpected error: {err}"
        );
        assert!(
            !kf.exists(),
            "braid.key must not be created when preflight fails"
        );
    }

    /*
     * Intent: `cmd_enroll_key_file` with `generate=true, dry_run=true`
     *   succeeds without ever reading a passphrase, probing the keyfile,
     *   or creating `braid.key` on disk.
     * Why it exists: dry-run is the user's safe "what would this do?"
     *   mode. It must short-circuit before any side effect (keyfile
     *   generation, passphrase prompt, header backup), and before any
     *   keyfile probe -- the keyfile does not exist yet by definition
     *   in --generate mode. The dry-run slot inventory probe is allowed
     *   because it is passphrase-free and side-effect-free. We assert
     *   the file is still absent afterward to prove the short-circuit
     *   fires before `generate_key_file`.
     * Scenario: user runs `braid enroll /mnt/usb --generate --dry-run`.
     */
    #[test]
    fn cmd_generate_dry_run_short_circuits() {
        let (tmp, paths) = isolated_paths();

        let kf = tmp.path().join("braid.key");

        let d1 = "/dev/disk/by-id/d1";
        let (uuid_req, uuid_out) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
        let (slot_req, slot_out) = enroll_luks_dump_slot1_empty(d1);

        // No passphrase mock and no TestKeyFile mock. If dry-run
        // regresses past the short-circuit, MockRunner returns
        // MissingMock and the test fails.
        let runner = MockRunner::default()
            .with_output(uuid_req, uuid_out)
            .with_luks_dump_text_luks2(d1)
            .with_mappers_closed(&["braid-disk1"])
            .with_output(slot_req, slot_out);
        let runner = enroll_with_mountpoint_ok(runner, tmp.path());

        let fs = enroll_fs(&[d1]);
        let membership = enroll_make_membership(&[("disk1", d1)]);

        cmd_enroll_key_file(
            &runner,
            &fs,
            &EnrollKeyFileParams {
                membership: &membership,
                key_file_path: &kf,
                generate: true,
                passphrase_stdin: false,
                passphrase_file: None,
                dry_run: true,
                paths: &paths,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        )
        .expect("dry-run must succeed without passphrase or mutations");

        assert!(
            !kf.exists(),
            "braid.key must remain absent after --generate --dry-run"
        );
    }

    #[test]
    // Intent: dry-run for --generate with 3 disks shows generate + 3× (enroll + backup).
    // Why: verifies compile_enroll_steps produces correct output for the common case.
    // Scenario: 3-disk pool, all present LUKS, --generate --dry-run.
    fn dry_run_render_enroll_generate_3_disks() {
        let candidates = vec![
            enroll_candidate("aaa", "/dev/disk/by-id/disk-aaa"),
            enroll_candidate("bbb", "/dev/disk/by-id/disk-bbb"),
            enroll_candidate("ccc", "/dev/disk/by-id/disk-ccc"),
        ];
        let (_state_dir, paths) = isolated_paths();
        let steps =
            compile_enroll_steps(&candidates, Path::new("/mnt/usb/braid.key"), true, &paths);
        let output = Step::render_dry_run(&steps);

        // 1 generate + 3× (enroll + backup) = 7 steps
        // generate has no command (1 line), others have 1 command each (2 lines each) = 1 + 6×2 = 13 lines
        assert_eq!(
            output.lines().count(),
            13,
            "expected 13 lines, got:\n{output}"
        );
        assert!(output.contains("generate keyfile"));
        assert!(output.contains("enroll keyfile"));
        assert!(output.contains("LUKS header backup"));
        assert!(output.contains("cryptsetup luksAddKey"));
        assert!(output.contains("cryptsetup luksHeaderBackup"));
    }

    #[test]
    // Intent: dry-run for existing keyfile with 2 disks omits generate step.
    // Why: verifies generate=false skips the keyfile generation step.
    // Scenario: 2-disk pool, existing keyfile, --dry-run (no --generate).
    fn dry_run_render_enroll_existing_keyfile() {
        let candidates = vec![
            enroll_candidate("aaa", "/dev/disk/by-id/disk-aaa"),
            enroll_candidate("bbb", "/dev/disk/by-id/disk-bbb"),
        ];
        let (_state_dir, paths) = isolated_paths();
        let steps =
            compile_enroll_steps(&candidates, Path::new("/mnt/usb/braid.key"), false, &paths);
        let output = Step::render_dry_run(&steps);

        // No generate step. 2× (enroll + backup) = 4 steps, each 2 lines = 8
        assert_eq!(
            output.lines().count(),
            8,
            "expected 8 lines, got:\n{output}"
        );
        assert!(!output.contains("generate keyfile"));
        assert!(output.contains("enroll keyfile"));
    }
}
