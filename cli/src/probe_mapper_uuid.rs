//! Shared LUKS UUID re-probe at observed mapper paths.
//!
//! The `cryptsetup status <mapper>` plus `cryptsetup luksUUID <backing>`
//! round-trip is the defense-in-depth seam used to prevent post-commit closes
//! of foreign disks: between the planning UUID classification and the
//! per-mapper `cryptsetup close`, an operator can manually open a different
//! disk under the same mapper name. Probing the mapper's current
//! backing-device LUKS UUID immediately before close lets each call site
//! demote the close to a skip on mismatch or unverifiable state.
//!
//! The helper centralizes the close-time ownership check for `remove`, live
//! `replace`, and post-maintenance replace recovery. Mismatch and unverifiable
//! probe paths emit the shared operator-facing `Warning:` text through
//! `emit_status`; inactive mappers are returned silently so each caller can
//! decide whether an already-closed mapper is surprising.

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::{BackingDevice, CryptsetupStatusOutput};
use crate::parse::{parse_cryptsetup_luks_uuid, parse_cryptsetup_status};
use crate::status_tag::emit_status;
use crate::types::{LuksUuid, MapperName};

/// Outcome of the close-time live mapper ownership probe.
///
/// Separates a clean "no active mapping" result from active-but-wrong or
/// unverifiable states, so recovery can treat absence as the normal
/// already-closed no-op while execute-time callers still warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapperOwnership {
    /// Active mapper backed by the expected LUKS UUID; the caller may close it.
    Owned,
    /// No active dm mapping for this name; caller decides whether to warn.
    Inactive,
    /// Active but wrong, or the probe could not prove ownership.
    Unverified,
}

/// Operator-facing note for execute-time close sites where inactive is odd.
pub(crate) fn warn_close_skipped_inactive(mapper: &MapperName, expected_uuid: &LuksUuid) {
    emit_status(&format!(
        "Warning: post-commit close skipped for mapper {mapper}: probe failed (mapper is inactive); expected LUKS UUID {expected}\n",
        mapper = mapper,
        expected = expected_uuid,
    ));
}

/// Probe the live LUKS UUID at the observed mapper and classify ownership.
/// Mismatch and unverifiable states emit a `Warning:` naming the mapper,
/// expected UUID, and observed UUID or probe error; inactive returns silently
/// so the call site can decide whether absence is normal.
pub(crate) fn probe_observed_mapper_uuid<R: CommandRunner>(
    runner: &R,
    mapper: &MapperName,
    expected_uuid: &LuksUuid,
) -> MapperOwnership {
    let status = match runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper.clone(),
    }) {
        Ok(out) => out,
        Err(e) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}\n",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            ));
            return MapperOwnership::Unverified;
        }
    };
    let backing_device = match parse_cryptsetup_status(&status) {
        Ok(CryptsetupStatusOutput::Active {
            backing: BackingDevice::Path(device),
        }) => device,
        Ok(CryptsetupStatusOutput::Inactive) => return MapperOwnership::Inactive,
        Ok(CryptsetupStatusOutput::Active {
            backing: BackingDevice::Null,
        }) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed (mapper backing device is unavailable (cryptsetup reports null)); expected LUKS UUID {expected}\n",
                mapper = mapper,
                expected = expected_uuid,
            ));
            return MapperOwnership::Unverified;
        }
        Err(e) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}\n",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            ));
            return MapperOwnership::Unverified;
        }
    };

    let probe = match runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: backing_device.to_owned(),
    }) {
        Ok(out) => out,
        Err(e) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}\n",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            ));
            return MapperOwnership::Unverified;
        }
    };
    match parse_cryptsetup_luks_uuid(&probe) {
        Ok(parsed) if parsed.uuid == *expected_uuid => MapperOwnership::Owned,
        Ok(parsed) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: expected LUKS UUID {expected} but observed {observed}\n",
                mapper = mapper,
                expected = expected_uuid,
                observed = parsed.uuid,
            ));
            MapperOwnership::Unverified
        }
        Err(e) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}\n",
                mapper = mapper,
                err = e,
                expected = expected_uuid,
            ));
            MapperOwnership::Unverified
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MapperOwnership, probe_observed_mapper_uuid};
    use crate::cmd::{CmdError, CmdRequest, MockRunner};
    use crate::test_fixtures::mock_ok;
    use crate::types::{LuksUuid, MapperName};

    fn test_uuid(seed: u64) -> LuksUuid {
        LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
            .expect("hand-padded UUID is canonical")
    }

    // Intent: a cryptsetup status runner error makes the close probe
    //   return Unverified before any backing-device UUID probe.
    // Why it exists: command-spawn or command-runner failures after
    //   the irreversible btrfs commit must demote to skip-close, not
    //   hard-error or close an unverified mapper.
    // Scenario: the observed mapper exists in the post-commit close
    //   path, but `cryptsetup status` cannot run.
    #[test]
    fn probe_returns_unverified_when_cryptsetup_status_runner_errs() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
        let expected = test_uuid(710);
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::CryptsetupStatus { .. } => {
                Some(Err(CmdError::Failed("cryptsetup status: not found".into())))
            }
            _ => None,
        });

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "status runner error must signal skip-close"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "status runner error must short-circuit before luksUUID"
        );
    }

    // Intent: an unparseable cryptsetup status body makes the close
    //   probe return Unverified before any backing-device UUID probe.
    // Why it exists: malformed status output cannot prove mapper
    //   ownership, so the fail-closed post-commit behavior is to skip
    //   the close.
    // Scenario: `cryptsetup status braid-WRONG` exits successfully but
    //   emits garbage instead of an active or inactive status shape.
    #[test]
    fn probe_returns_unverified_when_status_parse_fails() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
        let expected = test_uuid(711);
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.clone(),
            },
            mock_ok("cryptsetup status braid-WRONG", "garbage\n"),
        );

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "status parse error must signal skip-close"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "status parse error must short-circuit before luksUUID"
        );
    }

    // Intent: an active mapper whose status reports a null backing
    //   device makes the close probe return Unverified before luksUUID.
    // Why it exists: a null backing cannot be tied to the expected LUKS
    //   UUID, and closing it would be an ownership guess.
    // Scenario: `cryptsetup status braid-WRONG` reports the mapper as
    //   active, but the backing device line is `(null)`.
    #[test]
    fn probe_returns_unverified_when_backing_device_is_null() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
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

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "null backing device must signal skip-close"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "null backing device must short-circuit before luksUUID"
        );
    }

    // Intent: an inactive mapper makes the close probe return Inactive
    //   before any backing-device UUID probe.
    // Why it exists: a post-commit mapper can disappear between plan
    //   and close; that is already a safe skip, not evidence to close
    //   anything else.
    // Scenario: `cryptsetup status braid-WRONG` reports the mapper as
    //   inactive in the post-commit close path.
    #[test]
    fn probe_returns_inactive_when_mapper_is_inactive() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
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

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Inactive,
            "inactive mapper must return the caller-classified absence result"
        );
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::CryptsetupStatus {
                mapper: mapper.clone()
            }],
            "inactive mapper must short-circuit before luksUUID"
        );
    }

    // Intent: a backing-device luksUUID runner error makes the close
    //   probe return Unverified after the status probe identifies the backing
    //   device.
    // Why it exists: once the helper cannot read the backing LUKS UUID,
    //   it cannot prove the observed mapper still belongs to the
    //   journaled device.
    // Scenario: status resolves `braid-WRONG` to `/dev/vdc`, but
    //   `cryptsetup luksUUID /dev/vdc` fails.
    #[test]
    fn probe_returns_unverified_when_luks_uuid_runner_errs() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
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

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "luksUUID runner error must signal skip-close"
        );
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
    //   close probe return Unverified after the status probe identifies the
    //   backing device.
    // Why it exists: invalid UUID output is not proof that the observed
    //   mapper still owns the expected disk, so the post-commit close
    //   must be skipped.
    // Scenario: status resolves `braid-WRONG` to `/dev/vdc`, but
    //   `cryptsetup luksUUID /dev/vdc` emits `not-a-uuid`.
    #[test]
    fn probe_returns_unverified_when_luks_uuid_parse_fails() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
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

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "luksUUID parse error must signal skip-close"
        );
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

    // Intent: an active mapper whose backing luksUUID parses cleanly but differs
    //   from the expected UUID makes the close probe return Unverified, after both
    //   probes run.
    // Why it exists: this is the operator double-drift arm -- a foreign disk opened
    //   under the same mapper name reports a valid-but-wrong UUID. It is the one
    //   Unverified branch the other helper tests don't cover, and the close-skip
    //   guard at every call site (replace/remove/recover) hinges on it.
    // Scenario: status resolves braid-WRONG to /dev/vdc; `cryptsetup luksUUID
    //   /dev/vdc` returns a valid foreign UUID != the expected UUID.
    #[test]
    fn probe_returns_unverified_when_uuid_value_differs() {
        let mapper = MapperName::from_basename("braid-WRONG".into());
        let expected = test_uuid(716);
        let foreign = test_uuid(799);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: mapper.clone(),
                },
                mock_ok(
                    "cryptsetup status braid-WRONG",
                    "braid-WRONG is active and is in use.\n  type:    LUKS2\n  \
                     device:  /dev/vdc\n  mode:    read/write\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                mock_ok("cryptsetup luksUUID /dev/vdc", &format!("{foreign}\n")),
            );

        let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

        assert_eq!(
            ownership,
            MapperOwnership::Unverified,
            "a valid-but-different backing UUID must signal skip-close"
        );
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
            "value mismatch must run exactly one status probe and one UUID probe"
        );
    }
}
