use crate::cmd::CommandRunner;
use crate::config::mapper_name;
use crate::luks::{self, KeySlotState, LuksError, KEYFILE_SIZE, LUKS_SLOT_KEYFILE};
use crate::membership::PoolMembership;
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
    let ok = luks::verify_passphrase(runner, &first_by_id.0, passphrase)?;
    if !ok {
        return Err(EnrollKeyFileError::Validation(format!(
            "wrong passphrase (verified against {})",
            first_name
        )));
    }

    let mut plan = Vec::new();
    for (name, by_id) in candidates {
        // Check if keyfile already works (idempotent)
        if luks::verify_key_file(runner, &by_id.0, key_file_path)? {
            eprintln!("ok: {} — keyfile already enrolled", name);
            plan.push(DiskEnrollAction::AlreadyEnrolled {
                name: name.clone(),
                by_id: by_id.clone(),
            });
            continue;
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

        eprintln!("enroll: {} — will add keyfile to slot 1", name);
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
                eprintln!("ok: {} — keyfile enrolled in slot 1", name);

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

pub fn cmd_enroll_key_file<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    membership: &PoolMembership,
    key_file_path: &Path,
    generate: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    paths: &StatePaths,
) -> Result<(), EnrollKeyFileError> {
    if generate {
        // --generate: keyfile must NOT exist
        if key_file_path.exists() {
            return Err(EnrollKeyFileError::Validation(format!(
                "braid.key already exists at {}; remove it manually if you want to generate a new one",
                key_file_path.display()
            )));
        }

        // 1. Discover candidates
        let candidates = discover_enrollment_candidates(runner, fs, membership)?;

        // 2. Read passphrase
        let passphrase = luks::read_passphrase(passphrase_file, passphrase_stdin)?;

        // 3. Plan enrollment (preflight: passphrase + slot conflict detection)
        let plan = plan_enrollment(runner, &candidates, key_file_path, &passphrase)?;

        // 4. Only if preflight passes: generate keyfile
        generate_key_file(key_file_path)?;
        eprintln!("ok: generated {}", key_file_path.display());

        // 5. Apply enrollment
        apply_enrollment(runner, &plan, &passphrase, key_file_path, paths)?;
    } else {
        // Existing flow: keyfile must exist
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

        let candidates = discover_enrollment_candidates(runner, fs, membership)?;
        let passphrase = luks::read_passphrase(passphrase_file, passphrase_stdin)?;
        let plan = plan_enrollment(runner, &candidates, key_file_path, &passphrase)?;
        apply_enrollment(runner, &plan, &passphrase, key_file_path, paths)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::membership::DiskMember;
    use crate::probe::Filesystem;
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::BTreeMap;

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
            .with_output(req2, out2);
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
        let runner = MockRunner::default().with_output(req, out);
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
            .with_output(req2, out2);
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

        let paths = crate::state_paths::StatePaths::production();
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
}
