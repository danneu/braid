use crate::cmd::CommandRunner;
use crate::luks::{self, LuksError, VerifyOutcome};
use crate::secret::Passphrase;
use crate::status_tag::{
    CredentialKind, StatusTag, credential_ok_line, credential_wait_line, status_line,
};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVerifyTarget {
    pub name: String,
    pub device: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Credential<'a> {
    Passphrase(&'a Passphrase),
    KeyFile(&'a Path),
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
    use zeroize::Zeroizing;

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
}
