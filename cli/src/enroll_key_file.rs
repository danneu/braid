use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::mapper_name;
use crate::luks::{self, KeySlotState, LuksError, VerifyOutcome, KEYFILE_SIZE, LUKS_SLOT_KEYFILE};
use crate::membership::PoolMembership;
use crate::preflight;
use crate::probe::{self, Filesystem};
use crate::state_paths::StatePaths;
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
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskEnrollAction {
    AlreadyEnrolled { name: String, by_id: ByIdPath },
    NeedsEnroll { name: String, by_id: ByIdPath },
}

/// Discovery phase: iterate membership disks and collect present LUKS candidates.
/// Absent and non-LUKS disks are silently skipped.
/// Errors if zero candidates found.
fn discover_enrollment_candidates<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    membership: &PoolMembership,
) -> Result<Vec<(String, ByIdPath)>, EnrollKeyFileError> {
    let mut candidates = Vec::new();
    for (name, member) in &membership.disks {
        let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                eprintln!("skip: {} not present", name);
            }
            ConfigDiskState::PresentNotLuks => {
                eprintln!("skip: {} not LUKS-formatted", name);
            }
            ConfigDiskState::PresentLuks { .. } => {
                candidates.push((name.clone(), member.by_id.clone()));
            }
        }
    }

    if candidates.is_empty() {
        return Err(EnrollKeyFileError::Validation(
            "no present LUKS disks found to enroll keyfile into".into(),
        ));
    }

    Ok(candidates)
}

/// Planning phase: verify passphrase, then classify each candidate disk.
/// Returns an immutable plan — no mutations occur.
/// Fails immediately on wrong passphrase or slot-1 conflict.
fn plan_enrollment<R: CommandRunner>(
    runner: &R,
    candidates: &[(String, ByIdPath)],
    key_file_path: &Path,
    passphrase: &str,
) -> Result<Vec<DiskEnrollAction>, EnrollKeyFileError> {
    // Verify passphrase once against first candidate
    let (ref first_name, ref first_by_id) = candidates[0];
    match luks::verify_passphrase(runner, &first_by_id.0, passphrase)? {
        VerifyOutcome::Authenticated => {}
        VerifyOutcome::Rejected => {
            return Err(EnrollKeyFileError::Validation(format!(
                "wrong passphrase (verified against {})",
                first_name
            )));
        }
    }

    let mut plan = Vec::new();
    for (name, by_id) in candidates {
        // Check if keyfile already works (idempotent). Only `Authenticated`
        // means the keyfile is already installed in a slot -- `Rejected` is
        // the normal "not yet enrolled" signal. Any other non-zero exit
        // (busy/missing/generic) propagates via the `?` on verify_key_file
        // and must NOT be silently treated as "not enrolled" -- doing so
        // would let the flow proceed to slot preflight on a device that
        // may not even be readable.
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

        // Preflight: check slot 1 state
        let slot_state = luks::check_key_slot(runner, &by_id.0, LUKS_SLOT_KEYFILE)?;
        if slot_state == KeySlotState::Occupied {
            return Err(EnrollKeyFileError::Validation(format!(
                "slot 1 on {} ({}) is occupied by an unknown key. \
                 Remove it first with `cryptsetup luksKillSlot {} 1` then re-run enrollment.",
                name, by_id, by_id
            )));
        }

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
    Ok(())
}

/// Compile dry-run steps from discovered candidates.
pub fn compile_enroll_steps(
    candidates: &[(String, ByIdPath)],
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

pub fn cmd_enroll_key_file<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &EnrollKeyFileParams<'_>,
) -> Result<(), EnrollKeyFileError> {
    preflight::check_no_pending_operation(params.paths).map_err(EnrollKeyFileError::Validation)?;

    if params.generate {
        // --generate: keyfile must NOT exist
        if params.key_file_path.exists() {
            return Err(EnrollKeyFileError::Validation(format!(
                "braid.key already exists at {}; remove it manually if you want to generate a new one",
                params.key_file_path.display()
            )));
        }

        // 1. Discover candidates
        let candidates = discover_enrollment_candidates(runner, fs, params.membership)?;

        // Dry-run: show what would happen, skip passphrase + mutations
        if params.dry_run {
            let steps = compile_enroll_steps(
                &candidates,
                params.key_file_path,
                params.generate,
                params.paths,
            );
            Step::print_dry_run(&steps);
            return Ok(());
        }

        // 2. Read passphrase
        let passphrase = luks::read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

        // 3. Plan enrollment (preflight: passphrase + slot conflict detection)
        let plan = plan_enrollment(runner, &candidates, params.key_file_path, &passphrase)?;

        // 4. Only if preflight passes: generate keyfile
        generate_key_file(params.key_file_path)?;
        eprintln!("ok: generated {}", params.key_file_path.display());

        // 5. Apply enrollment
        apply_enrollment(
            runner,
            &plan,
            &passphrase,
            params.key_file_path,
            params.paths,
        )?;
    } else {
        // Existing flow: keyfile must exist
        if !params.key_file_path.exists() {
            return Err(EnrollKeyFileError::Validation(format!(
                "keyfile not found: {}",
                params.key_file_path.display()
            )));
        }
        let meta = std::fs::metadata(params.key_file_path).map_err(|e| {
            EnrollKeyFileError::Validation(format!(
                "cannot read keyfile {}: {e}",
                params.key_file_path.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(EnrollKeyFileError::Validation(format!(
                "keyfile is not a regular file: {}",
                params.key_file_path.display()
            )));
        }

        let candidates = discover_enrollment_candidates(runner, fs, params.membership)?;

        // Dry-run: show what would happen, skip passphrase + mutations
        if params.dry_run {
            let steps = compile_enroll_steps(
                &candidates,
                params.key_file_path,
                params.generate,
                params.paths,
            );
            Step::print_dry_run(&steps);
            return Ok(());
        }

        let passphrase = luks::read_passphrase(params.passphrase_file, params.passphrase_stdin)?;
        let plan = plan_enrollment(runner, &candidates, params.key_file_path, &passphrase)?;
        apply_enrollment(
            runner,
            &plan,
            &passphrase,
            params.key_file_path,
            params.paths,
        )?;
    }

    Ok(())
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

    // ---- discover_enrollment_candidates tests ----

    /*
     * Intent: verify that two present LUKS disks are both returned as candidates.
     * Why: ensures the discovery phase correctly identifies all eligible disks.
     * Scenario: normal 2-disk pool, both disks present and LUKS-formatted.
     */
    #[test]
    fn discover_two_present_luks_disks() {
        let (req1, out1) = luks_uuid_ok("/dev/disk/by-id/d1");
        let (req2, out2) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d1")
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2");
        let fs = MockFs::new(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);

        let result = discover_enrollment_candidates(&runner, &fs, &membership).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "disk1");
        assert_eq!(result[1].0, "disk2");
    }

    /*
     * Intent: verify skip semantics for absent disks.
     * Why: absent disks should be silently skipped, not cause errors.
     * Scenario: 2-disk pool but one disk is unplugged.
     */
    #[test]
    fn discover_one_absent_one_present() {
        let (req, out) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req, out)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2");
        let fs = MockFs::new(&["/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);

        let result = discover_enrollment_candidates(&runner, &fs, &membership).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "disk2");
    }

    /*
     * Intent: verify skip semantics for non-LUKS disks.
     * Why: disks without LUKS headers can't have keyfiles enrolled.
     * Scenario: config lists a disk that isn't LUKS-formatted yet.
     */
    #[test]
    fn discover_one_not_luks_one_luks() {
        let (req1, out1) = luks_uuid_not_luks("/dev/disk/by-id/d1");
        let (req2, out2) = luks_uuid_ok("/dev/disk/by-id/d2");
        let runner = MockRunner::default()
            .with_output(req1, out1)
            .with_output(req2, out2)
            .with_luks_dump_text_luks2("/dev/disk/by-id/d2");
        let fs = MockFs::new(&["/dev/disk/by-id/d1", "/dev/disk/by-id/d2"]);
        let membership = make_membership(&[
            ("disk1", "/dev/disk/by-id/d1"),
            ("disk2", "/dev/disk/by-id/d2"),
        ]);

        let result = discover_enrollment_candidates(&runner, &fs, &membership).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "disk2");
    }

    /*
     * Intent: verify error when all disks are absent.
     * Why: there's nothing to enroll into, the user should know.
     * Scenario: all disks unplugged.
     */
    #[test]
    fn discover_all_absent_errors() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let membership = make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let result = discover_enrollment_candidates(&runner, &fs, &membership);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
        );
    }

    /*
     * Intent: verify error when all present disks are non-LUKS.
     * Why: same as absent case — nothing to enroll into.
     * Scenario: disks are present but not yet LUKS-formatted.
     */
    #[test]
    fn discover_all_not_luks_errors() {
        let (req, out) = luks_uuid_not_luks("/dev/disk/by-id/d1");
        let runner = MockRunner::default().with_output(req, out);
        let fs = MockFs::new(&["/dev/disk/by-id/d1"]);
        let membership = make_membership(&[("disk1", "/dev/disk/by-id/d1")]);

        let result = discover_enrollment_candidates(&runner, &fs, &membership);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("no present LUKS disks found"),
            "unexpected error: {err}"
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

        let (tp_req, tp_stdin, tp_out) = test_passphrase_ok(d1, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(&runner, &candidates, Path::new(kf), pass).unwrap();
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

        let plan = plan_enrollment(&runner, &candidates, Path::new(kf), pass).unwrap();
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

        let (tp_req, tp_stdin, tp_out) = test_passphrase_ok(d1, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_ok(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld2_req, ld2_out) = luks_dump_slot1_empty(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let plan = plan_enrollment(&runner, &candidates, Path::new(kf), pass).unwrap();
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

        let result = plan_enrollment(&runner, &candidates, Path::new(kf), pass);
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

        let (tp_req, tp_stdin, tp_out) = test_passphrase_ok(d1, pass);
        let (tkf1_req, tkf1_out) = test_keyfile_fail(d1, kf);
        let (tkf2_req, tkf2_out) = test_keyfile_fail(d2, kf);
        let (ld1_req, ld1_out) = luks_dump_slot1_empty(d1);
        let (ld2_req, ld2_out) = luks_dump_slot1_occupied(d2);

        let runner = MockRunner::default()
            .with_output_stdin(tp_req, tp_stdin, tp_out)
            .with_output(tkf1_req, tkf1_out)
            .with_output(tkf2_req, tkf2_out)
            .with_output(ld1_req, ld1_out)
            .with_output(ld2_req, ld2_out);

        let candidates = vec![
            ("disk1".to_owned(), by_id(d1)),
            ("disk2".to_owned(), by_id(d2)),
        ];

        let result = plan_enrollment(&runner, &candidates, Path::new(kf), pass);
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

        let err = plan_enrollment(&runner, &candidates, Path::new(kf), pass)
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
