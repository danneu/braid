use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::mapper_name;
use crate::luks::{self, KEYFILE_SIZE, KeySlotState, LUKS_SLOT_KEYFILE, LuksError, VerifyOutcome};
use crate::membership::PoolMembership;
use crate::preflight;
use crate::preview::{self, NoteLevel, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{self, Filesystem};
use crate::state_paths::StatePaths;
use crate::status_tag::{CredentialKind, color_enabled_for_stderr, emit_credential_wait_line};
use crate::types::{ByIdPath, ConfigDiskState};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum EnrollKeyFileError {
    #[error("{0}")]
    Validation(String),
    #[error("luks error: {0}")]
    Luks(#[from] LuksError),
    #[error("probe error: {0}")]
    Probe(#[from] crate::probe::ProbeError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskEnrollAction {
    AlreadyEnrolled { name: String, by_id: ByIdPath },
    NeedsEnroll { name: String, by_id: ByIdPath },
}

/// Mode dispatch for `plan_enrollment`. The two modes share passphrase
/// verification and slot-1 conflict detection but differ on whether the
/// keyfile probe (`luks::verify_key_file`) runs.
///
/// `GenerateNew` must skip the keyfile probe -- the keyfile does not exist
/// yet, so probing it would always fail with "Failed to open key file" and
/// abort enrollment before the file is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnrollmentPlanMode {
    /// User-supplied keyfile already on disk. Probe it on each candidate
    /// to support idempotent re-enroll, then check slot 1 on disks that
    /// don't already have it.
    ExistingKeyfile,
    /// `--generate`: keyfile does not exist yet. Skip the keyfile probe
    /// entirely and only check slot 1 on every candidate -- every disk
    /// is by definition `NeedsEnroll`.
    GenerateNew,
}

pub type EnrollmentCandidate = (String, ByIdPath);
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
) -> EnrollmentCandidateDiscovery {
    let mut notes: Vec<PreviewNote> = Vec::new();
    let mut candidates: Vec<EnrollmentCandidate> = Vec::new();

    for (name, member) in &membership.disks {
        let probed = match probe::probe_config_disk(runner, fs, name, &member.by_id) {
            Ok(p) => p,
            Err(e) => return (notes, Err(e.into())),
        };
        match &probed.state {
            ConfigDiskState::Absent => {
                notes.push(PreviewNote::PerDisk {
                    name: name.clone(),
                    level: NoteLevel::Skip,
                    message: "not present".into(),
                });
            }
            ConfigDiskState::PresentNotLuks => {
                notes.push(PreviewNote::PerDisk {
                    name: name.clone(),
                    level: NoteLevel::Skip,
                    message: "not LUKS-formatted".into(),
                });
            }
            ConfigDiskState::PresentLuks { .. } => {
                candidates.push((name.clone(), member.by_id.clone()));
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

/// Verify the supplied passphrase against the first candidate disk.
/// Single source of truth for the "fail fast on wrong passphrase" preflight
/// shared by both planning modes. Wrong passphrase here would cause every
/// downstream `luksAddKey` to fail, so we surface it once up front rather
/// than partway through enrollment.
fn verify_first_candidate_passphrase<R: CommandRunner>(
    runner: &R,
    candidates: &[EnrollmentCandidate],
    passphrase: &str,
) -> Result<(), EnrollKeyFileError> {
    let (first_name, first_by_id) = &candidates[0];
    emit_credential_wait_line(
        CredentialKind::Passphrase,
        color_enabled_for_stderr(),
        first_name,
    );
    match luks::verify_passphrase(runner, &first_by_id.0, passphrase)? {
        VerifyOutcome::Authenticated => Ok(()),
        VerifyOutcome::Rejected => Err(EnrollKeyFileError::Validation(format!(
            "wrong passphrase (verified against {})",
            first_name
        ))),
    }
}

/// Slot-1 preflight: refuse to enroll if slot 1 is already occupied by
/// an unknown key. Same remediation regardless of mode.
fn check_slot_one_available<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
) -> Result<(), EnrollKeyFileError> {
    let slot_state = luks::check_key_slot(runner, &by_id.0, LUKS_SLOT_KEYFILE)?;
    if slot_state == KeySlotState::Occupied {
        return Err(EnrollKeyFileError::Validation(format!(
            "slot 1 on {} ({}) is occupied by an unknown key. \
             Remove it first with `cryptsetup luksKillSlot {} 1` then re-run enrollment.",
            name, by_id, by_id
        )));
    }
    Ok(())
}

/// Planning phase: verify passphrase, then classify each candidate disk.
/// Returns an immutable plan -- no mutations occur. Fails immediately on
/// wrong passphrase or slot-1 conflict.
///
/// Mode dispatch:
/// - `ExistingKeyfile`: probe the keyfile per-disk (idempotent re-enroll
///   collapses to `AlreadyEnrolled`); slot 1 only checked when the probe
///   was rejected.
/// - `GenerateNew`: keyfile does not exist yet, so the probe is skipped
///   entirely and every candidate gets a slot-1 check, producing only
///   `NeedsEnroll` actions.
fn plan_enrollment<R: CommandRunner>(
    runner: &R,
    candidates: &[EnrollmentCandidate],
    key_file_path: &Path,
    passphrase: &str,
    mode: EnrollmentPlanMode,
) -> Result<Vec<DiskEnrollAction>, EnrollKeyFileError> {
    verify_first_candidate_passphrase(runner, candidates, passphrase)?;

    let mut plan = Vec::new();
    for (i, (name, by_id)) in candidates.iter().enumerate() {
        if let EnrollmentPlanMode::ExistingKeyfile = mode {
            // Check if keyfile already works (idempotent). Only `Authenticated`
            // means the keyfile is already installed in a slot -- `Rejected` is
            // the normal "not yet enrolled" signal. Any other non-zero exit
            // (busy/missing/generic) propagates via the `?` on verify_key_file
            // and must NOT be silently treated as "not enrolled" -- doing so
            // would let the flow proceed to slot preflight on a device that
            // may not even be readable.
            emit_credential_wait_line(CredentialKind::KeyFile, color_enabled_for_stderr(), name);
            match luks::verify_key_file(runner, &by_id.0, key_file_path)? {
                VerifyOutcome::Authenticated => {
                    eprintln!("ok: {} -- keyfile already enrolled", name);
                    plan.push(DiskEnrollAction::AlreadyEnrolled {
                        name: name.clone(),
                        by_id: by_id.clone(),
                    });
                    continue;
                }
                VerifyOutcome::Rejected => {}
            }
        }

        // Per-disk passphrase verify: every disk that will be mutated has its
        // passphrase verified during planning. Without this, a divergent
        // passphrase on a non-first disk (e.g. user ran `cryptsetup
        // luksChangeKey` on disk2 out-of-band) would not surface until the
        // apply phase, which leaves the pool partially mutated. The first
        // candidate is already covered by `verify_first_candidate_passphrase`
        // above, which also handles the all-`AlreadyEnrolled` case where
        // this loop never reaches the verify (every iter takes the `continue`
        // above).
        if i > 0 {
            emit_credential_wait_line(CredentialKind::Passphrase, color_enabled_for_stderr(), name);
            match luks::verify_passphrase(runner, &by_id.0, passphrase)? {
                VerifyOutcome::Authenticated => {}
                VerifyOutcome::Rejected => {
                    return Err(EnrollKeyFileError::Validation(format!(
                        "wrong passphrase on {}",
                        name
                    )));
                }
            }
        }

        check_slot_one_available(runner, name, by_id)?;

        eprintln!("enroll: {} -- will add keyfile to slot 1", name);
        plan.push(DiskEnrollAction::NeedsEnroll {
            name: name.clone(),
            by_id: by_id.clone(),
        });
    }

    Ok(plan)
}

/// Apply phase: execute mutations for NeedsEnroll items only.
fn apply_enrollment<R: CommandRunner>(
    runner: &R,
    plan: &[DiskEnrollAction],
    passphrase: &str,
    key_file_path: &Path,
    paths: &StatePaths,
) -> Result<(), EnrollKeyFileError> {
    apply_enrollment_with_backup_dir(
        runner,
        plan,
        passphrase,
        key_file_path,
        &paths.luks_headers_dir(),
    )
}

fn apply_enrollment_with_backup_dir<R: CommandRunner>(
    runner: &R,
    plan: &[DiskEnrollAction],
    passphrase: &str,
    key_file_path: &Path,
    backup_dir: &Path,
) -> Result<(), EnrollKeyFileError> {
    let mut enrolled = 0u32;
    let mut already = 0u32;

    for action in plan {
        match action {
            DiskEnrollAction::AlreadyEnrolled { .. } => {
                already += 1;
            }
            DiskEnrollAction::NeedsEnroll { name, by_id } => {
                luks::enroll_key_file(runner, &by_id.0, passphrase, key_file_path)?;
                eprintln!("ok: {} -- keyfile enrolled in slot 1", name);

                let mn = mapper_name(name);
                let backup_path = luks::backup_luks_header_to(runner, &by_id.0, &mn.0, backup_dir)?;
                eprintln!("LUKS header backed up: {}", backup_path.display());

                enrolled += 1;
            }
        }
    }

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
    let mut buf = vec![0u8; KEYFILE_SIZE];
    rng.read_exact(&mut buf)?;

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    f.write_all(&buf)?;
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
            description: format!("generate keyfile → {}", key_file_path.display()),
            commands: vec![],
        });
    }

    for (name, by_id) in candidates {
        let mn = mapper_name(name);
        steps.push(Step {
            risk: "safe",
            description: format!("enroll keyfile → LUKS slot 1 on {}", by_id),
            commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                device: by_id.0.clone(),
                key_file_path: key_file_path.display().to_string(),
            }],
        });
        let backup_path = paths
            .luks_headers_dir()
            .join(format!("{}.luksheader", mn.0));
        steps.push(Step {
            risk: "safe",
            description: format!("LUKS header backup → {}", backup_path.display()),
            commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                device: by_id.0.clone(),
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
}

/// Report returned by `plan_enroll`. On the `Ok` branch, `notes` is
/// always empty and all accumulated per-disk skip notes live on
/// `EnrollPlan.notes` (the single source of truth for successful
/// preview + real-run stderr prelude). On the `Err` branch, `notes`
/// carries the discovery notes accumulated before the failure so the
/// caller can render them to stderr before the error -- preserving
/// today's "skip: <name> not present" lines that printed before the
/// "no present LUKS disks" error.
#[derive(Debug)]
pub struct EnrollPlanReport {
    pub notes: Vec<PreviewNote>,
    pub result: Result<EnrollPlan, EnrollKeyFileError>,
}

/// Dry-run preview source of truth for `braid enroll` plus the
/// execute inputs pre-computed during planning. `notes` + `steps` are
/// both rendered by `preview()`; `execute()` renders `notes` to stderr
/// (using `STDERR_STYLE`) before any mutation.
#[derive(Debug)]
pub struct EnrollPlan {
    pub notes: Vec<PreviewNote>,
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
        eprint!(
            "{}",
            preview::render_notes_for_stderr_with(
                &self.notes,
                Self::STDERR_STYLE,
                crate::status_tag::color_enabled_for_stderr(),
            )
        );

        let passphrase = luks::read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

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
            generate_key_file(params.key_file_path)?;
            eprintln!("ok: generated {}", params.key_file_path.display());
        }

        apply_enrollment(
            runner,
            &enrollment,
            &passphrase,
            params.key_file_path,
            params.paths,
        )?;

        Ok(())
    }
}

/// No-context preview-generation failure for bad `--key-file` paths.
/// Lives in `plan_enroll` so cmd-level code has a single planner
/// entry point. Failures here have no accumulated notes.
fn validate_key_file_path(key_file_path: &Path, generate: bool) -> Result<(), EnrollKeyFileError> {
    if generate {
        if key_file_path.exists() {
            return Err(EnrollKeyFileError::Validation(format!(
                "braid.key already exists at {}; remove it manually if you want to generate a new one",
                key_file_path.display()
            )));
        }
    } else {
        if !key_file_path.exists() {
            return Err(EnrollKeyFileError::Validation(format!(
                "keyfile not found: {}",
                key_file_path.display()
            )));
        }
        let meta = std::fs::metadata(key_file_path).map_err(|e| {
            EnrollKeyFileError::Validation(format!(
                "cannot read keyfile {}: {e}",
                key_file_path.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(EnrollKeyFileError::Validation(format!(
                "keyfile is not a regular file: {}",
                key_file_path.display()
            )));
        }
    }
    Ok(())
}

/// Plan a `braid enroll` run. Owns the pending-op preflight,
/// keyfile-path validation, and pre-passphrase discovery. Per-disk
/// skip notes land on `EnrollPlan.notes` when discovery produces at
/// least one candidate, or on `EnrollPlanReport.notes` when the
/// planner bails (e.g. no candidates, mid-loop probe error).
pub fn plan_enroll<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    membership: &PoolMembership,
    key_file_path: &Path,
    generate: bool,
    paths: &StatePaths,
) -> EnrollPlanReport {
    if let Err(msg) = preflight::check_no_pending_operation(paths) {
        return EnrollPlanReport {
            notes: Vec::new(),
            result: Err(EnrollKeyFileError::Validation(msg)),
        };
    }

    if let Err(e) = validate_key_file_path(key_file_path, generate) {
        return EnrollPlanReport {
            notes: Vec::new(),
            result: Err(e),
        };
    }

    let (notes, discovery) = discover_enrollment_candidates(runner, fs, membership);
    match discovery {
        Ok(candidates) => {
            let steps = compile_enroll_steps(&candidates, key_file_path, generate, paths);
            EnrollPlanReport {
                notes: Vec::new(),
                result: Ok(EnrollPlan {
                    notes,
                    steps,
                    candidates,
                    generate,
                }),
            }
        }
        Err(e) => EnrollPlanReport {
            notes,
            result: Err(e),
        },
    }
}

pub fn cmd_enroll_key_file<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &EnrollKeyFileParams<'_>,
) -> Result<(), EnrollKeyFileError> {
    let report = plan_enroll(
        runner,
        fs,
        params.membership,
        params.key_file_path,
        params.generate,
        params.paths,
    );
    let plan = match report.result {
        Ok(p) => p,
        Err(e) => {
            // Preserved-context failure: accumulated skip notes render
            // to stderr before the error message, mirroring today's
            // `eprintln!("skip: ...")` + validation-error sequence on
            // the no-candidates path.
            eprint!(
                "{}",
                preview::render_notes_for_stderr_with(
                    &report.notes,
                    EnrollPlan::STDERR_STYLE,
                    crate::status_tag::color_enabled_for_stderr(),
                )
            );
            return Err(e);
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
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::membership::DiskMember;
    use crate::probe::Filesystem;
    use crate::types::ByIdPath;
    use std::collections::BTreeMap;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    fn by_id(path: &str) -> ByIdPath {
        ByIdPath(path.to_owned())
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn make_membership(disks: &[(&str, &str)]) -> PoolMembership {
        let mut map = BTreeMap::new();
        for (key, path) in disks {
            map.insert(key.to_string(), DiskMember::from_by_id(by_id(path)));
        }
        PoolMembership { disks: map }
    }

    // -- Mock response helpers --

    fn luks_uuid_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksUuid {
                device: device.to_owned(),
            },
            ok_raw(
                &format!("cryptsetup luksUUID {device}"),
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n",
            ),
        )
    }

    fn luks_uuid_not_luks(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksUuid {
                device: device.to_owned(),
            },
            err_raw(
                &format!("cryptsetup luksUUID {device}"),
                4,
                "Device is not a valid LUKS device.",
            ),
        )
    }

    fn luks_dump_slot1_empty(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDump {
                device: device.to_owned(),
            },
            ok_raw(
                &format!("cryptsetup luksDump {device}"),
                r#"{"keyslots":{"0":{"type":"luks2"}}}"#,
            ),
        )
    }

    fn luks_dump_slot1_occupied(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDump {
                device: device.to_owned(),
            },
            ok_raw(
                &format!("cryptsetup luksDump {device}"),
                r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
            ),
        )
    }

    fn test_passphrase_ok(
        device: &str,
        passphrase: &str,
    ) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestPassphrase {
                device: device.to_owned(),
            },
            passphrase.as_bytes().to_vec(),
            ok_raw(&format!("cryptsetup open --test-passphrase {device}"), ""),
        )
    }

    fn test_passphrase_fail(
        device: &str,
        passphrase: &str,
    ) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestPassphrase {
                device: device.to_owned(),
            },
            passphrase.as_bytes().to_vec(),
            err_raw(
                &format!("cryptsetup open --test-passphrase {device}"),
                2,
                "No key available with this passphrase.",
            ),
        )
    }

    fn test_keyfile_ok(device: &str, key_file: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestKeyFile {
                device: device.to_owned(),
                key_file_path: key_file.to_owned(),
            },
            ok_raw("cryptsetup open --test-passphrase --key-file", ""),
        )
    }

    fn test_keyfile_fail(device: &str, key_file: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupTestKeyFile {
                device: device.to_owned(),
                key_file_path: key_file.to_owned(),
            },
            err_raw("cryptsetup open --test-passphrase --key-file", 2, "No key"),
        )
    }

    fn enroll_ok(
        device: &str,
        key_file: &str,
        passphrase: &str,
    ) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksAddKeyFile {
                device: device.to_owned(),
                key_file_path: key_file.to_owned(),
            },
            passphrase.as_bytes().to_vec(),
            ok_raw("cryptsetup luksAddKey", ""),
        )
    }

    // ---- plan_enroll discovery tests ----
    //
    // These tests exercise `plan_enroll(..., generate=true, ...)` because
    // `--generate` requires the keyfile path to NOT exist, so the temp
    // path (never created) satisfies the pre-discovery validation with
    // zero setup. Mode choice is irrelevant to the discovery behavior
    // being asserted here.

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
        let (req1, out1) = luks_uuid_ok("/dev/disk/by-id/d1");
        let (req2, out2) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        let fs = MockFs::new(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let report = plan_enroll(&runner, &fs, &membership, &kf, true, &paths);
        assert!(
            report.notes.is_empty(),
            "report.notes should be empty on success"
        );
        let plan = report.result.expect("plan_enroll should succeed");
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(plan.candidates[0].0, "disk1");
        assert_eq!(plan.candidates[1].0, "disk2");
        assert!(
            plan.notes.is_empty(),
            "plan.notes should be empty when all candidates present"
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
        let (req, out) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = MockFs::new(&["/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let report = plan_enroll(&runner, &fs, &membership, &kf, true, &paths);
        assert!(
            report.notes.is_empty(),
            "report.notes should be empty on success"
        );
        let plan = report
            .result
            .expect("plan_enroll should succeed with one candidate");
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].0, "disk2");
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
        let (req1, out1) = luks_uuid_not_luks("/dev/disk/by-id/d1");
        let (req2, out2) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = MockFs::new(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let report = plan_enroll(&runner, &fs, &membership, &kf, true, &paths);
        assert!(report.notes.is_empty());
        let plan = report.result.expect("plan_enroll should succeed");
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].0, "disk2");
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
     *   skip notes preserved on `report.notes` -- the preserved-context
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
        let fs = MockFs::new(&[]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let report = plan_enroll(&runner, &fs, &membership, &kf, true, &paths);
        let err = report.result.expect_err("expected no-candidates error");
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
        );
        assert_eq!(
            report.notes.len(),
            2,
            "both skip notes must survive the Err branch"
        );
        for (i, name) in ["disk1", "disk2"].iter().enumerate() {
            match &report.notes[i] {
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
        let (req, out) = luks_uuid_not_luks("/dev/disk/by-id/d1");
        let runner = MockRunner::default().with_output(req, out);
        let fs = MockFs::new(&["/dev/disk/by-id/d1"]);
        let membership = make_membership(&[("disk1", "/dev/disk/by-id/d1")]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let report = plan_enroll(&runner, &fs, &membership, &kf, true, &paths);
        let err = report.result.expect_err("expected no-candidates error");
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
        );
        assert_eq!(report.notes.len(), 1);
        match &report.notes[0] {
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
        let (req, out) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2")
            .with_mapper_closed("braid-disk2");
        let fs = MockFs::new(&["/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);
        let (tmp, paths) = test_paths();
        let kf = tmp.path().join("braid.key");

        let plan = plan_enroll(&runner, &fs, &membership, &kf, true, &paths)
            .result
            .expect("plan_enroll should succeed");

        let rendered_stdout = plan.preview().render();
        assert!(
            rendered_stdout.contains("[skip] disk disk1: not present\n"),
            "bracketed skip missing from preview stdout: {rendered_stdout}"
        );

        let rendered_stderr =
            preview::render_notes_for_stderr(&plan.notes, EnrollPlan::STDERR_STYLE);
        assert_eq!(rendered_stderr, "skip: disk1 not present\n");
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            DiskEnrollAction::NeedsEnroll {
                name: "disk1".to_owned(),
                by_id: by_id(d1),
            }
        );
        assert_eq!(
            plan[1],
            DiskEnrollAction::NeedsEnroll {
                name: "disk2".to_owned(),
                by_id: by_id(d2),
            }
        );
    }

    /*
     * Intent: verify plan correctly identifies disks with keyfile already enrolled.
     * Why: re-enrollment should be idempotent — no mutation needed.
     * Scenario: keyfile already in slot 1 on all disks.
     */
    #[test]
    fn plan_all_already_enrolled() {
        let d1 = "/dev/disk/by-id/d1";
        let d2 = "/dev/disk/by-id/d2";
        let kf = "/tmp/braid.key";
        let pass = "testpass";

        let (tp_req, tp_stdin, tp_out) = test_passphrase_ok(d1, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_ok(d2, kf);

        let runner = MockRunner::default()
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(
            matches!(&plan[0], DiskEnrollAction::AlreadyEnrolled { name, .. } if name == "disk1")
        );
        assert!(
            matches!(&plan[1], DiskEnrollAction::AlreadyEnrolled { name, .. } if name == "disk2")
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld2_req, ld2_out) = luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
            EnrollmentPlanMode::ExistingKeyfile,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(
            matches!(&plan[0], DiskEnrollAction::AlreadyEnrolled { name, .. } if name == "disk1")
        );
        assert!(matches!(&plan[1], DiskEnrollAction::NeedsEnroll { name, .. } if name == "disk2"));
    }

    /*
     * Intent: verify wrong passphrase is detected early.
     * Why: wrong passphrase would cause all luksAddKey calls to fail — catch it up front.
     * Scenario: user mistyped their passphrase.
     */
    #[test]
    fn plan_wrong_passphrase_errors() {
        let d1 = "/dev/disk/by-id/d1";
        let kf = "/tmp/braid.key";
        let pass = "wrongpass";

        let (tp_req, tp_stdin, tp_out) = test_passphrase_fail(d1, pass);
        let runner = MockRunner::default().with_output_stdin(tp_req, tp_stdin, tp_out);

        let candidates = vec![("disk1".to_owned(), by_id(d1))];

        let result = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
            EnrollmentPlanMode::ExistingKeyfile,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("wrong passphrase"),
            "unexpected error: {err}"
        );
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_ok(d2, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_occupied(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let result = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
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

        let (tp_req, tp_stdin, tp_out) = test_passphrase_ok(d1, pass);

        // test-keyfile exits 5 (EBUSY) -- this is the regression signal.
        let tkf_req = CmdRequest::CryptsetupTestKeyFile {
            device: d1.to_owned(),
            key_file_path: kf.to_owned(),
        };
        let tkf_out = err_raw(
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

        let candidates = vec![("disk1".to_owned(), by_id(d1))];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_empty(d2);

        // Deliberately NO CryptsetupTestKeyFile mocks. If `GenerateNew`
        // mode regresses and calls `verify_key_file`, MockRunner returns
        // MissingMock and this test fails.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
            EnrollmentPlanMode::GenerateNew,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            DiskEnrollAction::NeedsEnroll {
                name: "disk1".to_owned(),
                by_id: by_id(d1),
            }
        );
        assert_eq!(
            plan[1],
            DiskEnrollAction::NeedsEnroll {
                name: "disk2".to_owned(),
                by_id: by_id(d2),
            }
        );
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_ok(d2, pass);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_occupied(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
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
     *   rejected during planning, before any disk is mutated.
     * Why it exists: the two-phase enroll refactor's stated guarantee is
     *   "no partial mutation on preflight failure". This holds for slot-1
     *   conflicts because `check_key_slot` runs per disk, and held for
     *   wrong-passphrase only against the first candidate. A divergent
     *   passphrase on disk2 would pass planning and partial-mutate at
     *   apply time. This test pins the per-disk passphrase verify in
     *   the planner. No `CryptsetupLuksDump` mock is seeded for disk2 --
     *   if the planner regresses and reaches disk2's slot-1 check after
     *   skipping the per-disk passphrase verify, MockRunner returns
     *   MissingMock and this test fails loudly rather than passing
     *   silently.
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_fail(d1, kf);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_fail(d2, pass);

        // Deliberately NO `CryptsetupLuksDump` mock for d2. If the planner
        // regresses (e.g. skips the per-disk passphrase verify on the
        // second candidate), it will reach `check_slot_one_available` on
        // d2 and MockRunner will fail with MissingMock. That signals the
        // regression — the test must NOT pass silently in that case.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(ld1_req, ld1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
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

        let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_fail(d2, pass);

        // No keyfile-probe mocks (GenerateNew skips that branch). No
        // `CryptsetupLuksDump` mock for d2 -- if the planner reaches
        // d2's slot-1 check, the per-disk passphrase verify regressed
        // and the test must fail loudly via MissingMock.
        let runner = MockRunner::default()
            .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
            .with_output(ld1_req, ld1_out)
            .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let err = plan_enrollment(
            &runner,
            &candidates,
            Path::new(kf),
            pass,
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
        let backup_dir = tempfile::tempdir().unwrap();

        let (e1_req, e1_stdin, e1_out) = enroll_ok(d1, kf, pass);
        let (e2_req, e2_stdin, e2_out) = enroll_ok(d2, kf, pass);

        let runner = MockRunner::default()
            .with_output_stdin(e1_req, e1_stdin, e1_out)
            .with_output_stdin(e2_req, e2_stdin, e2_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d1.to_owned(),
                    backup_path: backup_dir
                        .path()
                        .join("braid-disk1.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw("cryptsetup luksHeaderBackup", ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d2.to_owned(),
                    backup_path: backup_dir
                        .path()
                        .join("braid-disk2.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw("cryptsetup luksHeaderBackup", ""),
            );

        let plan = vec![
            DiskEnrollAction::NeedsEnroll {
                name: "disk1".to_owned(),
                by_id: by_id(d1),
            },
            DiskEnrollAction::NeedsEnroll {
                name: "disk2".to_owned(),
                by_id: by_id(d2),
            },
        ];

        apply_enrollment_with_backup_dir(&runner, &plan, pass, Path::new(kf), backup_dir.path())
            .unwrap();
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
            name: "disk1".to_owned(),
            by_id: by_id(d1),
        }];

        let (_state_dir, paths) = test_paths();
        apply_enrollment(&runner, &plan, pass, Path::new(kf), &paths).unwrap();
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
        let backup_dir = tempfile::tempdir().unwrap();

        // Only d2 should have enroll called — d1 is AlreadyEnrolled
        let (e2_req, e2_stdin, e2_out) = enroll_ok(d2, kf, pass);
        let runner = MockRunner::default()
            .with_output_stdin(e2_req, e2_stdin, e2_out)
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: d2.to_owned(),
                    backup_path: backup_dir
                        .path()
                        .join("braid-disk2.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw("cryptsetup luksHeaderBackup", ""),
            );

        let plan = vec![
            DiskEnrollAction::AlreadyEnrolled {
                name: "disk1".to_owned(),
                by_id: by_id(d1),
            },
            DiskEnrollAction::NeedsEnroll {
                name: "disk2".to_owned(),
                by_id: by_id(d2),
            },
        ];

        apply_enrollment_with_backup_dir(&runner, &plan, pass, Path::new(kf), backup_dir.path())
            .unwrap();
    }

    // ---- generate_key_file tests ----

    /*
     * Intent: verify --generate rejects existing keyfile.
     * Why: --generate must not silently overwrite an existing keyfile.
     * Scenario: user runs --generate when braid.key already exists on USB.
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        // Create a pending-op journal
        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                disks: std::collections::BTreeMap::new(),
            },
        );
        crate::journal::write_journal(&paths, &journal).unwrap();

        // No mock commands — if enroll reaches cryptsetup, MockRunner will panic
        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let membership = make_membership(&[("d1", "/dev/disk/by-id/d1")]);
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
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("interrupted operation"),
            "expected 'interrupted operation' in: {err}"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        let kf = tmp.path().join("braid.key");
        let pass_path = tmp.path().join("pass");
        std::fs::write(&pass_path, "wrongpass\n").unwrap();

        let d1 = "/dev/disk/by-id/d1";
        let (uuid_req, uuid_out) = luks_uuid_ok(d1);
        let (tp_req, tp_stdin, tp_out) = test_passphrase_fail(d1, "wrongpass");

        let runner = MockRunner::default()
            .with_output(uuid_req, uuid_out)
            .with_luks_dump_text_luks2(d1)
            .with_mappers_closed(&["braid-disk1"])
            .with_output_stdin(tp_req, tp_stdin, tp_out);

        let fs = MockFs::new(&[d1]);
        let membership = make_membership(&[("disk1", d1)]);

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
     *   in --generate mode. We assert the file is still absent afterward
     *   to prove the short-circuit fires before `generate_key_file`.
     * Scenario: user runs `braid enroll /mnt/usb --generate --dry-run`.
     */
    #[test]
    fn cmd_generate_dry_run_short_circuits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());

        let kf = tmp.path().join("braid.key");

        let d1 = "/dev/disk/by-id/d1";
        let (uuid_req, uuid_out) = luks_uuid_ok(d1);

        // No passphrase mock, no TestKeyFile mock, no slot dump. If
        // dry-run regresses past the short-circuit, MockRunner returns
        // MissingMock and the test fails.
        let runner = MockRunner::default()
            .with_output(uuid_req, uuid_out)
            .with_luks_dump_text_luks2(d1)
            .with_mappers_closed(&["braid-disk1"]);

        let fs = MockFs::new(&[d1]);
        let membership = make_membership(&[("disk1", d1)]);

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
            ("aaa".to_owned(), by_id("/dev/disk/by-id/disk-aaa")),
            ("bbb".to_owned(), by_id("/dev/disk/by-id/disk-bbb")),
            ("ccc".to_owned(), by_id("/dev/disk/by-id/disk-ccc")),
        ];
        let (_state_dir, paths) = test_paths();
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
            ("aaa".to_owned(), by_id("/dev/disk/by-id/disk-aaa")),
            ("bbb".to_owned(), by_id("/dev/disk/by-id/disk-bbb")),
        ];
        let (_state_dir, paths) = test_paths();
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
