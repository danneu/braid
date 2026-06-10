use crate::cmd::CommandRunner;
use crate::luks::{self, LuksError, VerifyOutcome};
use crate::membership::{self, PoolMembership};
use crate::secret::Passphrase;
use crate::status_tag::{StatusTag, status_line};
use crate::types::{ByIdPath, DiskName, PoolDevice};
use std::path::Path;

/// One disk a credential is checked against: a cosmetic display `name`
/// plus the `device` path cryptsetup verification runs on. Fields are
/// private so a target can only be minted by `existing_pool_member`
/// (UUID->DiskName join) or `named_candidate` (validated operator input)
/// -- a mapper-derived display name is unconstructable (decision 024,
/// principle 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVerifyTarget {
    name: String,
    device: String,
}

impl CredentialVerifyTarget {
    /// Decision-024 present-device display rule, enforced at the type
    /// boundary: resolve a live pool member's display name through the
    /// UUID->DiskName join so a drifted mapper can never leak into the
    /// credential-verify line. Verification targets the live `underlying`
    /// path.
    pub fn existing_pool_member(membership: &PoolMembership, device: &PoolDevice) -> Self {
        Self {
            name: membership::present_device_name(membership, device),
            device: device.underlying.clone(),
        }
    }

    /// Operator-attested target: the name is an already-validated
    /// `DiskName` (never a mapper basename), the device the by-id setup
    /// handle.
    pub fn named_candidate(name: &DiskName, device: &ByIdPath) -> Self {
        Self {
            name: name.as_str().to_owned(),
            device: device.as_str().to_owned(),
        }
    }

    /// Display name for verify rows and rejection messages (cosmetic
    /// only; identity is `device`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Path cryptsetup verification runs on: a live member's `underlying`
    /// path or a candidate's by-id handle.
    pub fn device(&self) -> &str {
        &self.device
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Credential<'a> {
    Passphrase(&'a Passphrase),
    KeyFile(&'a Path),
}

/// Cheap `Copy` display discriminant for credential-verification rows;
/// deliberately separate from `Credential<'a>`, which borrows the live secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialKind {
    Passphrase,
    KeyFile,
}

impl CredentialKind {
    fn label(self) -> &'static str {
        match self {
            CredentialKind::Passphrase => "passphrase",
            CredentialKind::KeyFile => "keyfile",
        }
    }
}

#[derive(Debug)]
pub enum CredentialVerifyError {
    Rejected {
        target: CredentialVerifyTarget,
    },
    Luks {
        target: CredentialVerifyTarget,
        source: luks::LuksError,
    },
}

fn credential_wait_line(kind: CredentialKind, color_enabled: bool, name: &str) -> String {
    status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("{}: checking against {name}...", kind.label()),
    )
}

fn credential_ok_line(kind: CredentialKind, color_enabled: bool, name: &str) -> String {
    status_line(
        StatusTag::Ok,
        color_enabled,
        &format!("{}: accepted by {name}", kind.label()),
    )
}

pub fn verify_credential_for_targets<R: CommandRunner>(
    runner: &R,
    targets: &[CredentialVerifyTarget],
    credential: Credential<'_>,
    color_enabled: bool,
    mut emit: impl FnMut(&str),
) -> Result<(), CredentialVerifyError> {
    let kind = match credential {
        Credential::Passphrase(_) => CredentialKind::Passphrase,
        Credential::KeyFile(_) => CredentialKind::KeyFile,
    };

    for target in targets {
        let wait_line = credential_wait_line(kind, color_enabled, &target.name);
        emit(&wait_line);

        let outcome = match credential {
            Credential::Passphrase(passphrase) => {
                luks::verify_passphrase(runner, &target.device, passphrase)
            }
            Credential::KeyFile(path) => luks::verify_key_file(runner, &target.device, path),
        };

        match outcome {
            Ok(VerifyOutcome::Authenticated) => {
                let ok_line = credential_ok_line(kind, color_enabled, &target.name);
                emit(&ok_line);
            }
            Ok(VerifyOutcome::Rejected) => {
                return Err(CredentialVerifyError::Rejected {
                    target: target.clone(),
                });
            }
            Err(source) => {
                return Err(CredentialVerifyError::Luks {
                    target: target.clone(),
                    source,
                });
            }
        }
    }

    Ok(())
}

/// Probe whether a candidate disk already has the keyfile installed.
///
/// Sibling to `verify_credential_for_targets`: same wait-line idiom,
/// but rejection is informational ("not yet enrolled") rather than
/// fatal. Emits exactly one `[wait]` row, then exactly one closer
/// (`[ok]` on Authenticated, `[skip]` on Rejected). On `LuksError`
/// the wait closes via the caller's error propagation per Principle
/// 13.
///
/// Used by `braid enroll`'s dry-run preview (`plan_enroll`) and
/// real-run planner (`plan_enrollment`, ExistingKeyfile mode) so both
/// paths render byte-identical rows through the `emit_status` test
/// seam.
pub fn probe_keyfile_enrollment<R: CommandRunner>(
    runner: &R,
    target: &CredentialVerifyTarget,
    key_file_path: &Path,
    color_enabled: bool,
    mut emit: impl FnMut(&str),
) -> Result<VerifyOutcome, LuksError> {
    emit(&credential_wait_line(
        CredentialKind::KeyFile,
        color_enabled,
        &target.name,
    ));
    let outcome = luks::verify_key_file(runner, &target.device, key_file_path)?;
    let (tag, body) = match outcome {
        VerifyOutcome::Authenticated => (
            StatusTag::Ok,
            format!("keyfile: already enrolled on {}", target.name),
        ),
        VerifyOutcome::Rejected => (
            StatusTag::Skip,
            format!("keyfile: not yet enrolled on {}", target.name),
        ),
    };
    emit(&status_line(tag, color_enabled, &body));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::membership::DiskMember;
    use crate::types::{Devid, LuksUuid, MapperName};
    use zeroize::Zeroizing;

    // Test-module seed allocation: cli/src/credential_verify.rs uses 600-609.
    fn test_uuid(seed: u64) -> LuksUuid {
        LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
            .expect("hand-padded UUID is canonical")
    }

    fn member(name: &str, by_id: &str) -> DiskMember {
        DiskMember::new(
            DiskName::parse(name).expect("valid disk name in fixture"),
            ByIdPath::parse(by_id).expect("valid by-id path in fixture"),
        )
    }

    fn zpass(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(Zeroizing::new(s.to_owned()))
    }

    fn raw(exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup open --test-passphrase".to_owned(),
            stdout: String::new(),
            stderr: if exit_status == 0 {
                String::new()
            } else {
                format!("exit {exit_status}")
            },
            exit_status,
        }
    }

    fn targets() -> Vec<CredentialVerifyTarget> {
        vec![
            CredentialVerifyTarget {
                name: "disk1".to_owned(),
                device: "/dev/disk/by-id/one".to_owned(),
            },
            CredentialVerifyTarget {
                name: "disk2".to_owned(),
                device: "/dev/disk/by-id/two".to_owned(),
            },
            CredentialVerifyTarget {
                name: "disk3".to_owned(),
                device: "/dev/disk/by-id/three".to_owned(),
            },
        ]
    }

    /// Expected emit sequence when every target authenticates: per
    /// target, a wait line followed by an ok line.
    fn expected_wait_ok_pairs(
        targets: &[CredentialVerifyTarget],
        kind: CredentialKind,
        color_enabled: bool,
    ) -> Vec<String> {
        let mut out = Vec::with_capacity(targets.len() * 2);
        for target in targets {
            out.push(credential_wait_line(kind, color_enabled, &target.name));
            out.push(credential_ok_line(kind, color_enabled, &target.name));
        }
        out
    }

    fn passphrase_runner(
        targets: &[CredentialVerifyTarget],
        exits: &[i32],
    ) -> (MockRunner, Vec<CmdRequest>) {
        let mut runner = MockRunner::default();
        let mut requests = Vec::new();
        for (target, exit) in targets.iter().zip(exits.iter().copied()) {
            let req = CmdRequest::CryptsetupTestPassphrase {
                device: target.device.clone(),
            };
            requests.push(req.clone());
            runner = runner.with_output_stdin(req, b"secret".to_vec(), raw(exit));
        }
        (runner, requests)
    }

    fn key_file_runner(
        targets: &[CredentialVerifyTarget],
        exits: &[i32],
    ) -> (MockRunner, Vec<CmdRequest>) {
        let mut runner = MockRunner::default();
        let mut requests = Vec::new();
        for (target, exit) in targets.iter().zip(exits.iter().copied()) {
            let req = CmdRequest::CryptsetupTestKeyFile {
                device: target.device.clone(),
                key_file_path: "/run/braid.key".to_owned(),
            };
            requests.push(req.clone());
            runner = runner.with_output(req, raw(exit));
        }
        (runner, requests)
    }

    #[derive(Debug, Clone, Copy)]
    enum CredentialCase {
        Passphrase,
        KeyFile,
    }

    impl CredentialCase {
        fn kind(self) -> CredentialKind {
            match self {
                Self::Passphrase => CredentialKind::Passphrase,
                Self::KeyFile => CredentialKind::KeyFile,
            }
        }
    }

    fn with_case(
        case: CredentialCase,
        targets: &[CredentialVerifyTarget],
        exits: &[i32],
        run: impl FnOnce(MockRunner, Credential<'_>, Vec<CmdRequest>),
    ) {
        match case {
            CredentialCase::Passphrase => {
                let (runner, requests) = passphrase_runner(targets, exits);
                let passphrase = zpass("secret");
                run(runner, Credential::Passphrase(&passphrase), requests);
            }
            CredentialCase::KeyFile => {
                let (runner, requests) = key_file_runner(targets, exits);
                run(
                    runner,
                    Credential::KeyFile(Path::new("/run/braid.key")),
                    requests,
                );
            }
        }
    }

    // Intent: credential verification rows use the shared
    //   status-line renderer and fixed wording for both credential
    //   kinds.
    // Why it exists: every command that validates a passphrase or
    //   keyfile should fill the silent cryptsetup delay with
    //   byte-identical rows.
    // Scenario: passphrase and keyfile wait/ok lines render in
    //   plain mode.
    #[test]
    fn credential_wait_line_formats_known_credentials() {
        assert_eq!(
            credential_wait_line(CredentialKind::Passphrase, false, "disk1"),
            "[wait] passphrase: checking against disk1...\n"
        );
        assert_eq!(
            credential_wait_line(CredentialKind::KeyFile, false, "disk1"),
            "[wait] keyfile: checking against disk1...\n"
        );
        assert_eq!(
            credential_ok_line(CredentialKind::Passphrase, false, "disk1"),
            "[ok]   passphrase: accepted by disk1\n"
        );
        assert_eq!(
            credential_ok_line(CredentialKind::KeyFile, false, "disk1"),
            "[ok]   keyfile: accepted by disk1\n"
        );
    }

    #[test]
    fn verify_credential_for_targets_authenticates_all_targets_in_order() {
        for case in [CredentialCase::Passphrase, CredentialCase::KeyFile] {
            for color_enabled in [false, true] {
                let targets = targets();
                with_case(
                    case,
                    &targets,
                    &[0, 0, 0],
                    |runner, credential, requests| {
                        let mut emits = Vec::new();

                        verify_credential_for_targets(
                            &runner,
                            &targets,
                            credential,
                            color_enabled,
                            |line| emits.push(line.to_owned()),
                        )
                        .expect("all targets should authenticate");

                        assert_eq!(
                            emits,
                            expected_wait_ok_pairs(&targets, case.kind(), color_enabled)
                        );
                        assert_eq!(runner.requests(), requests);
                    },
                );
            }
        }
    }

    #[test]
    fn verify_credential_for_targets_stops_at_first_rejection() {
        for case in [CredentialCase::Passphrase, CredentialCase::KeyFile] {
            let targets = targets();
            with_case(
                case,
                &targets,
                &[0, 2, 0],
                |runner, credential, requests| {
                    let mut emits = Vec::new();

                    let err = verify_credential_for_targets(
                        &runner,
                        &targets,
                        credential,
                        false,
                        |line| emits.push(line.to_owned()),
                    )
                    .expect_err("target 2 should reject");

                    assert!(matches!(
                        err,
                        CredentialVerifyError::Rejected { target } if target == targets[1]
                    ));
                    // First target authenticates -> wait+ok pair; second target
                    // rejects -> wait line only (no terminal ok). The rejected
                    // wait is closed by the caller's error propagation per
                    // Principle 13.
                    let mut expected = expected_wait_ok_pairs(&targets[..1], case.kind(), false);
                    expected.push(credential_wait_line(case.kind(), false, &targets[1].name));
                    assert_eq!(emits, expected);
                    assert_eq!(runner.requests(), requests[..2]);
                },
            );
        }
    }

    #[test]
    fn verify_credential_for_targets_returns_luks_on_non_auth_exit() {
        for case in [CredentialCase::Passphrase, CredentialCase::KeyFile] {
            let targets = targets();
            with_case(
                case,
                &targets,
                &[0, 1, 0],
                |runner, credential, requests| {
                    let mut emits = Vec::new();

                    let err = verify_credential_for_targets(
                        &runner,
                        &targets,
                        credential,
                        false,
                        |line| emits.push(line.to_owned()),
                    )
                    .expect_err("target 2 should return LUKS error");

                    assert!(matches!(
                        err,
                        CredentialVerifyError::Luks { target, .. } if target == targets[1]
                    ));
                    // First target authenticates -> wait+ok; second target
                    // returns Luks error -> wait only. The Luks wait closes
                    // via the caller's error path.
                    let mut expected = expected_wait_ok_pairs(&targets[..1], case.kind(), false);
                    expected.push(credential_wait_line(case.kind(), false, &targets[1].name));
                    assert_eq!(emits, expected);
                    assert_eq!(runner.requests(), requests[..2]);
                },
            );
        }
    }

    #[test]
    fn verify_credential_for_targets_empty_list_is_ok() {
        let runner = MockRunner::default();
        let targets: Vec<CredentialVerifyTarget> = Vec::new();
        let mut emits = Vec::new();
        let passphrase = zpass("secret");

        verify_credential_for_targets(
            &runner,
            &targets,
            Credential::Passphrase(&passphrase),
            false,
            |line| emits.push(line.to_owned()),
        )
        .expect("empty target list should be ok");

        assert!(emits.is_empty());
        assert!(runner.requests().is_empty());
    }

    // Intent: probe_keyfile_enrollment emits the [wait] then [ok]
    //   pair when the keyfile authenticates, in both color modes,
    //   and returns Authenticated.
    // Why it exists: the helper's row contract is the unification
    //   point for the dry-run and real-run probe sites; without
    //   byte-pinned wait+ok output, a regression in either site
    //   would diverge from the VM-test wording without surfacing
    //   in Rust tests.
    // Scenario: a single-disk probe finds the keyfile already in
    //   slot 1.
    #[test]
    fn probe_keyfile_enrollment_authenticated_emits_wait_then_already_enrolled() {
        for color_enabled in [false, true] {
            let targets = targets();
            let (runner, requests) = key_file_runner(&targets[..1], &[0]);
            let mut emits = Vec::new();

            let outcome = probe_keyfile_enrollment(
                &runner,
                &targets[0],
                Path::new("/run/braid.key"),
                color_enabled,
                |line| emits.push(line.to_owned()),
            )
            .expect("keyfile should authenticate");

            assert_eq!(outcome, VerifyOutcome::Authenticated);
            assert_eq!(
                emits,
                vec![
                    credential_wait_line(CredentialKind::KeyFile, color_enabled, &targets[0].name),
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("keyfile: already enrolled on {}", targets[0].name),
                    ),
                ]
            );
            assert_eq!(runner.requests(), requests);
        }
    }

    // Intent: probe_keyfile_enrollment emits the [wait] then [skip]
    //   pair when the keyfile is rejected, and returns Rejected
    //   without erroring.
    // Why it exists: pins that rejection closes the wait via a
    //   visible [skip] row rather than error propagation, so
    //   plan_enroll's dry-run preview can continue iterating
    //   candidates without losing per-disk context.
    // Scenario: a single-disk probe finds the keyfile is not yet
    //   enrolled (cryptsetup exit 2).
    #[test]
    fn probe_keyfile_enrollment_rejected_emits_wait_then_not_yet_enrolled() {
        let targets = targets();
        let (runner, requests) = key_file_runner(&targets[..1], &[2]);
        let mut emits = Vec::new();

        let outcome = probe_keyfile_enrollment(
            &runner,
            &targets[0],
            Path::new("/run/braid.key"),
            false,
            |line| emits.push(line.to_owned()),
        )
        .expect("rejection is not an error path");

        assert_eq!(outcome, VerifyOutcome::Rejected);
        assert_eq!(
            emits,
            vec![
                credential_wait_line(CredentialKind::KeyFile, false, &targets[0].name),
                status_line(
                    StatusTag::Skip,
                    false,
                    &format!("keyfile: not yet enrolled on {}", targets[0].name),
                ),
            ]
        );
        assert_eq!(runner.requests(), requests);
    }

    // Intent: a non-auth cryptsetup exit (e.g. EBUSY exit 5) from
    //   the keyfile probe surfaces as Err(LuksError::OpenFailed)
    //   with no closer row -- the wait closes via the caller's
    //   error propagation per Principle 13.
    // Why it exists: a regression that swallows the LuksError into
    //   a synthetic [skip] row would let dry-run / real-run probe
    //   busy devices and pretend they're "not yet enrolled",
    //   matching the original pre-VerifyOutcome bug.
    // Scenario: a stale dm-crypt mapper holds the backing device
    //   busy during a probe.
    #[test]
    fn probe_keyfile_enrollment_luks_error_emits_wait_only_and_propagates() {
        let targets = targets();
        let (runner, _requests) = key_file_runner(&targets[..1], &[5]);
        let mut emits = Vec::new();

        let err = probe_keyfile_enrollment(
            &runner,
            &targets[0],
            Path::new("/run/braid.key"),
            false,
            |line| emits.push(line.to_owned()),
        )
        .expect_err("non-auth exit must surface as LuksError");

        match err {
            LuksError::OpenFailed { exit_code, .. } => {
                assert_eq!(exit_code, 5);
            }
            other => panic!("expected OpenFailed, got {other:?}"),
        }
        assert_eq!(
            emits,
            vec![credential_wait_line(
                CredentialKind::KeyFile,
                false,
                &targets[0].name,
            )]
        );
    }

    // Intent: existing_pool_member resolves the display name through the
    //   UUID->DiskName membership join and verifies against the live
    //   backing path.
    // Why it exists: the constructor is now the only way to mint a
    //   live-member verify target; if it regressed to the mapper basename
    //   or the by-id handle, every credential-verify line would re-violate
    //   decision 024 under mapper drift -- exactly the original bug.
    // Scenario: a pool member is open under a drifted mapper
    //   (braid-WRONG) while membership names its UUID 'disk1'.
    #[test]
    fn existing_pool_member_resolves_drifted_mapper_through_uuid() {
        let uuid = test_uuid(600);
        let mut membership = PoolMembership::empty();
        membership
            .insert(uuid.clone(), member("disk1", "/dev/disk/by-id/ata-K"))
            .unwrap();
        let device = PoolDevice {
            mapper: MapperName("braid-WRONG".into()),
            luks_uuid: uuid,
            devid: Devid::new(1),
            underlying: "/dev/vdb".into(),
        };

        let target = CredentialVerifyTarget::existing_pool_member(&membership, &device);

        assert_eq!(
            target.name(),
            "disk1",
            "drifted mapper must resolve to the membership name via UUID"
        );
        assert_eq!(
            target.device(),
            "/dev/vdb",
            "verification must target the live backing path, not the mapper or by-id"
        );
    }

    // Intent: a live device whose UUID is absent from membership falls
    //   back to the full mapper basename as its display name.
    // Why it exists: pins the constructor to present_device_name's
    //   foreign fallback -- the full 'braid-WRONG', never stripped to
    //   'WRONG', which would fabricate an operator-looking name.
    // Scenario: a foreign LUKS device is open under a braid-* mapper
    //   while membership is empty.
    #[test]
    fn existing_pool_member_foreign_uuid_falls_back_to_mapper_basename() {
        let membership = PoolMembership::empty();
        let foreign = PoolDevice {
            mapper: MapperName("braid-WRONG".into()),
            luks_uuid: test_uuid(601),
            devid: Devid::new(2),
            underlying: "/dev/vdc".into(),
        };

        let target = CredentialVerifyTarget::existing_pool_member(&membership, &foreign);

        assert_eq!(
            target.name(),
            "braid-WRONG",
            "foreign UUID -> full mapper basename, NOT stripped to 'WRONG'"
        );
        assert_eq!(target.device(), "/dev/vdc");
    }

    // Intent: named_candidate carries the validated DiskName and by-id
    //   handle through to the accessors unchanged.
    // Why it exists: the operator-input constructor must not transform
    //   either value -- the name is already validated at the boundary,
    //   and the by-id path is the handle cryptsetup verification runs on.
    // Scenario: enroll/mount/add/replace build a candidate target from
    //   operator-supplied CLI input.
    #[test]
    fn named_candidate_round_trips_validated_inputs() {
        let name = DiskName::parse("disk7").expect("valid disk name in fixture");
        let by_id =
            ByIdPath::parse("/dev/disk/by-id/ata-NEW").expect("valid by-id path in fixture");

        let target = CredentialVerifyTarget::named_candidate(&name, &by_id);

        assert_eq!(target.name(), "disk7");
        assert_eq!(target.device(), "/dev/disk/by-id/ata-NEW");
    }
}
