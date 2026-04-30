use crate::cmd::CommandRunner;
use crate::luks::{self, VerifyOutcome};
use crate::status_tag::{CredentialKind, credential_ok_line, credential_wait_line};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVerifyTarget {
    pub name: String,
    pub device: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Credential<'a> {
    Passphrase(&'a str),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};

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
                run(runner, Credential::Passphrase("secret"), requests);
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

        verify_credential_for_targets(
            &runner,
            &targets,
            Credential::Passphrase("secret"),
            false,
            |line| emits.push(line.to_owned()),
        )
        .expect("empty target list should be ok");

        assert!(emits.is_empty());
        assert!(runner.requests().is_empty());
    }
}
