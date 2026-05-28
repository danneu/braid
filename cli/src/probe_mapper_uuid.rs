//! Shared LUKS UUID re-probe at observed mapper paths.
//!
//! The `cryptsetup status <mapper>` plus `cryptsetup luksUUID <backing>`
//! round-trip is the defense-in-depth seam used to prevent post-commit
//! closes of foreign disks: between the planning UUID classification and
//! the per-mapper `cryptsetup close`, an operator can manually open a
//! different disk under the same mapper name. Probing the mapper's
//! current backing-device LUKS UUID immediately before close lets each
//! call site demote the close to a logged-warning skip on mismatch.
//!
//! Today the same helper exists inline in `remove.rs` and `replace.rs`;
//! Phase 4 of the LUKS-UUID-as-identity migration lifts the body here
//! so the two existing callers and the upcoming recovery-side callers
//! (`finish_uncommitted_replace_recovery`, addressed in Phase 4b) share a
//! single source of truth. The helper is logger-coupled by design -- every
//! failure path emits the operator-facing Warning text and returns `false`
//! so the caller proceeds to skip the close. Phase 4b will reuse this body.

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::{BackingDevice, CryptsetupStatusOutput};
use crate::parse::{parse_cryptsetup_luks_uuid, parse_cryptsetup_status};
use crate::types::{LuksUuid, MapperName};

/// Probe the live LUKS UUID at the observed mapper and require it to
/// equal `expected_uuid`. Returns `true` if the probe matched (caller
/// should proceed to close); returns `false` on mismatch or any probe
/// failure (caller should skip close). The failure path emits a
/// `Warning: ...` to stderr naming the mapper, expected UUID, and the
/// observed UUID or probe error -- the message a future operator runs
/// to reason about why the close was skipped.
pub(crate) fn probe_observed_mapper_uuid<R: CommandRunner>(
    runner: &R,
    mapper: &MapperName,
    expected_uuid: &LuksUuid,
) -> bool {
    let status = match runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper.clone(),
    }) {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            );
            return false;
        }
    };
    let backing_device = match parse_cryptsetup_status(&status) {
        Ok(CryptsetupStatusOutput::Active {
            backing: BackingDevice::Path(device),
        }) => device,
        Ok(CryptsetupStatusOutput::Inactive) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed (mapper is inactive); expected LUKS UUID {expected}",
                mapper = mapper,
                expected = expected_uuid,
            );
            return false;
        }
        Ok(CryptsetupStatusOutput::Active {
            backing: BackingDevice::Null,
        }) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed (mapper backing device is unavailable (cryptsetup reports null)); expected LUKS UUID {expected}",
                mapper = mapper,
                expected = expected_uuid,
            );
            return false;
        }
        Err(e) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            );
            return false;
        }
    };

    let probe = match runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: backing_device.to_owned(),
    }) {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            );
            return false;
        }
    };
    match parse_cryptsetup_luks_uuid(&probe) {
        Ok(parsed) if parsed.uuid == *expected_uuid => true,
        Ok(parsed) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: expected LUKS UUID {expected} but observed {observed}",
                mapper = mapper,
                expected = expected_uuid,
                observed = parsed.uuid,
            );
            false
        }
        Err(e) => {
            eprintln!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::probe_observed_mapper_uuid;
    use crate::cmd::{CmdError, CmdRequest, MockRunner};
    use crate::test_fixtures::mock_ok;
    use crate::types::{LuksUuid, MapperName};

    fn test_uuid(seed: u64) -> LuksUuid {
        LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
            .expect("hand-padded UUID is canonical")
    }

    // Intent: a cryptsetup status runner error makes the close probe
    //   return false before any backing-device UUID probe.
    // Why it exists: command-spawn or command-runner failures after
    //   the irreversible btrfs commit must demote to skip-close, not
    //   hard-error or close an unverified mapper.
    // Scenario: the observed mapper exists in the post-commit close
    //   path, but `cryptsetup status` cannot run.
    #[test]
    fn probe_returns_false_when_cryptsetup_status_runner_errs() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(710);
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::CryptsetupStatus { .. } => {
                Some(Err(CmdError::Failed("cryptsetup status: not found".into())))
            }
            _ => None,
        });

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "status runner error must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "status runner error must short-circuit before luksUUID"
        );
    }

    // Intent: an unparseable cryptsetup status body makes the close
    //   probe return false before any backing-device UUID probe.
    // Why it exists: malformed status output cannot prove mapper
    //   ownership, so the fail-closed post-commit behavior is to skip
    //   the close.
    // Scenario: `cryptsetup status braid-WRONG` exits successfully but
    //   emits garbage instead of an active or inactive status shape.
    #[test]
    fn probe_returns_false_when_status_parse_fails() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(711);
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.clone(),
            },
            mock_ok("cryptsetup status braid-WRONG", "garbage\n"),
        );

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "status parse error must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "status parse error must short-circuit before luksUUID"
        );
    }

    // Intent: an active mapper whose status reports a null backing
    //   device makes the close probe return false before luksUUID.
    // Why it exists: a null backing cannot be tied to the expected LUKS
    //   UUID, and closing it would be an ownership guess.
    // Scenario: `cryptsetup status braid-WRONG` reports the mapper as
    //   active, but the backing device line is `(null)`.
    #[test]
    fn probe_returns_false_when_backing_device_is_null() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(712);
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.clone(),
            },
            mock_ok(
                "cryptsetup status braid-WRONG",
                "braid-WRONG is active and is in use.\n  type:    LUKS2\n  device:  (null)\n  mode:    read/write\n",
            ),
        );

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "null backing device must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "null backing device must short-circuit before luksUUID"
        );
    }

    // Intent: an inactive mapper makes the close probe return false
    //   before any backing-device UUID probe.
    // Why it exists: a post-commit mapper can disappear between plan
    //   and close; that is already a safe skip, not evidence to close
    //   anything else.
    // Scenario: `cryptsetup status braid-WRONG` reports the mapper as
    //   inactive in the post-commit close path.
    #[test]
    fn probe_returns_false_when_mapper_is_inactive() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(713);
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.clone(),
            },
            mock_ok(
                "cryptsetup status braid-WRONG",
                "/dev/mapper/braid-WRONG is inactive.\n",
            ),
        );

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "inactive mapper must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "inactive mapper must short-circuit before luksUUID"
        );
    }

    // Intent: a backing-device luksUUID runner error makes the close
    //   probe return false after the status probe identifies the backing
    //   device.
    // Why it exists: once the helper cannot read the backing LUKS UUID,
    //   it cannot prove the observed mapper still belongs to the
    //   journaled device.
    // Scenario: status resolves `braid-WRONG` to `/dev/vdc`, but
    //   `cryptsetup luksUUID /dev/vdc` fails.
    #[test]
    fn probe_returns_false_when_luks_uuid_runner_errs() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(714);
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-WRONG" => {
                Some(Ok(mock_ok(
                    "cryptsetup status braid-WRONG",
                    "braid-WRONG is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdc\n  mode:    read/write\n",
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdc" => Some(Err(
                CmdError::Failed("cryptsetup luksUUID /dev/vdc: device gone".into()),
            )),
            _ => None,
        });

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "luksUUID runner error must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupStatus {
                    mapper: mapper.clone()
                },
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into()
                },
            ],
            "luksUUID runner error must run exactly one status probe and one UUID probe"
        );
    }

    // Intent: an unparseable backing-device luksUUID body makes the
    //   close probe return false after the status probe identifies the
    //   backing device.
    // Why it exists: invalid UUID output is not proof that the observed
    //   mapper still owns the expected disk, so the post-commit close
    //   must be skipped.
    // Scenario: status resolves `braid-WRONG` to `/dev/vdc`, but
    //   `cryptsetup luksUUID /dev/vdc` emits `not-a-uuid`.
    #[test]
    fn probe_returns_false_when_luks_uuid_parse_fails() {
        let mapper = MapperName("braid-WRONG".into());
        let expected = test_uuid(715);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: mapper.clone(),
                },
                mock_ok(
                    "cryptsetup status braid-WRONG",
                    "braid-WRONG is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdc\n  mode:    read/write\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                mock_ok("cryptsetup luksUUID /dev/vdc", "not-a-uuid\n"),
            );

        let matched = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert!(!matched, "luksUUID parse error must signal skip-close");
        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupStatus {
                    mapper: mapper.clone()
                },
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into()
                },
            ],
            "luksUUID parse error must run exactly one status probe and one UUID probe"
        );
    }
}
