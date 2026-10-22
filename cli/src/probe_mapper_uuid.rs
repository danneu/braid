//! Shared LUKS UUID re-probe at observed mapper paths.
//!
//! Reading the LUKS UUID an observed mapper records (its dm UUID, see
//! `cli/src/dm_identity.rs`) is the defense-in-depth seam used to prevent
//! post-commit closes of foreign disks: between the planning UUID
//! classification and the per-mapper `cryptsetup close`, an operator can
//! manually open a different disk under the same mapper name. Probing the
//! mapper's current LUKS UUID immediately before close lets each call site
//! demote the close to a skip on mismatch or unverifiable state. The identity
//! lives on the mapper, so a removed disk that is already gone still closes.
//!
//! The helper centralizes the close-time ownership check for `remove`, live
//! `replace`, and post-maintenance replace recovery. Mismatch and unverifiable
//! probe paths emit the shared operator-facing `Warning:` text through
//! `emit_status`; inactive mappers are returned silently so each caller can
//! decide whether an already-closed mapper is surprising.

use crate::dm_identity::{UnprovenMapper, mapper_luks_uuid};
use crate::filesystem::Filesystem;
use crate::status_tag::emit_status;
use crate::types::{LuksUuid, MapperName};

/// Outcome of the close-time live mapper ownership probe.
///
/// Separates a clean "no active mapping" result from active-but-wrong or
/// unverifiable states, so recovery can treat absence as the normal
/// already-closed no-op while execute-time callers still warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapperOwnership {
    /// Active mapper recording the expected LUKS UUID; the caller may close it.
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

/// Probe the LUKS UUID the observed mapper records and classify ownership.
/// Mismatch and unverifiable states emit a `Warning:` naming the mapper,
/// expected UUID, and observed UUID or probe error; inactive returns silently
/// so the call site can decide whether absence is normal.
pub(crate) fn probe_observed_mapper_uuid<F: Filesystem + ?Sized>(
    fs: &F,
    mapper: &MapperName,
    expected_uuid: &LuksUuid,
) -> MapperOwnership {
    match mapper_luks_uuid(fs, mapper) {
        Ok(observed) if observed == *expected_uuid => MapperOwnership::Owned,
        Ok(observed) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: expected LUKS UUID {expected} but observed {observed}\n",
                mapper = mapper,
                expected = expected_uuid,
                observed = observed,
            ));
            MapperOwnership::Unverified
        }
        Err(UnprovenMapper::Absent) => MapperOwnership::Inactive,
        Err(unproven @ (UnprovenMapper::NotLuks { .. } | UnprovenMapper::Unreadable { .. })) => {
            emit_status(&format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed ({err}); expected LUKS UUID {expected}\n",
                mapper = mapper,
                err = unproven,
                expected = expected_uuid,
            ));
            MapperOwnership::Unverified
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MapperOwnership, probe_observed_mapper_uuid, warn_close_skipped_inactive};
    use crate::filesystem::Filesystem;
    use crate::test_fixtures::{MockFs, test_uuid};
    use crate::types::{LuksUuid, MapperName};

    fn wrong() -> MapperName {
        MapperName::from_basename("braid-WRONG".into())
    }

    fn probe<F: Filesystem + ?Sized>(fs: &F, expected: &LuksUuid) -> (MapperOwnership, String) {
        let mut ownership = None;
        let output = crate::status_tag::testing::capture_with_color(false, || {
            ownership = Some(probe_observed_mapper_uuid(fs, &wrong(), expected));
        });
        (ownership.expect("probe ran"), output)
    }

    // Intent: an active mapper recording the expected LUKS UUID is Owned and
    //   the probe stays silent.
    // Why it exists: the post-commit close must proceed for the mapper the
    //   journal names, including under mapper drift.
    // Scenario: the journaled old disk is open as `braid-WRONG`, whose dm UUID
    //   records the journaled UUID.
    #[test]
    fn probe_returns_owned_when_recorded_uuid_matches() {
        let expected = test_uuid(716);
        let fs = MockFs::storage(vec![]).with_luks_dm("braid-WRONG", expected.as_str());

        assert_eq!(
            probe(&fs, &expected),
            (MapperOwnership::Owned, String::new())
        );
    }

    // Intent: no device-mapper device by the mapper's name is Inactive, and
    //   the probe stays silent.
    // Why it exists: recovery treats an already-closed mapper as a normal
    //   no-op, while execute-time callers warn through
    //   `warn_close_skipped_inactive`.
    // Scenario: the mapper was closed before the post-commit close ran.
    #[test]
    fn probe_returns_inactive_when_no_mapping_has_the_name() {
        let expected = test_uuid(711);
        let fs = MockFs::storage(vec![]).with_luks_dm("braid-other", expected.as_str());

        assert_eq!(
            probe(&fs, &expected),
            (MapperOwnership::Inactive, String::new())
        );
    }

    // Intent: an active mapper recording a different LUKS UUID is Unverified
    //   and warns with both UUIDs.
    // Why it exists: between planning and close, an operator can open a
    //   different disk under the same mapper name; closing it would lock a
    //   foreign disk.
    // Scenario: `braid-WRONG` now maps a different container.
    #[test]
    fn probe_returns_unverified_when_recorded_uuid_differs() {
        let expected = test_uuid(712);
        let observed = test_uuid(713);
        let fs = MockFs::storage(vec![]).with_luks_dm("braid-WRONG", observed.as_str());

        assert_eq!(
            probe(&fs, &expected),
            (
                MapperOwnership::Unverified,
                format!(
                    "Warning: post-commit close skipped for mapper braid-WRONG: expected LUKS UUID {expected} but observed {observed}\n"
                ),
            )
        );
    }

    // Intent: an active mapper with no LUKS dm UUID is Unverified and warns
    //   with the dm UUID it found.
    // Why it exists: a mapping cryptsetup did not create as LUKS proves no
    //   container, so it must never be closed as the journaled disk.
    // Scenario: an operator opened a plain dm-crypt mapping as `braid-WRONG`.
    #[test]
    fn probe_returns_unverified_when_mapping_is_not_luks() {
        let expected = test_uuid(714);
        let fs = MockFs::storage(vec![]).with_raw_dm("braid-WRONG", "CRYPT-PLAIN-braid-WRONG");

        assert_eq!(
            probe(&fs, &expected),
            (
                MapperOwnership::Unverified,
                format!(
                    "Warning: post-commit close skipped for mapper braid-WRONG: probe failed (device-mapper UUID \"CRYPT-PLAIN-braid-WRONG\" is not a cryptsetup LUKS UUID); expected LUKS UUID {expected}\n"
                ),
            )
        );
    }

    // Intent: an unreadable device-mapper table is Unverified, never Inactive.
    // Why it exists: Inactive lets recovery skip the close silently, so a read
    //   failure must not pass for an already-closed mapper.
    // Scenario: listing `/sys/block` fails with a permission error.
    #[test]
    fn probe_returns_unverified_when_dm_table_is_unreadable() {
        struct DeniedSysBlock;
        impl Filesystem for DeniedSysBlock {
            fn exists(&self, _path: &str) -> bool {
                false
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
            fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            }
            fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
                unreachable!("nothing to read after a failed listing")
            }
            fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
                unreachable!("read-only probe")
            }
        }
        let expected = test_uuid(715);

        assert_eq!(
            probe(&DeniedSysBlock, &expected),
            (
                MapperOwnership::Unverified,
                format!(
                    "Warning: post-commit close skipped for mapper braid-WRONG: probe failed (cannot read /sys/block: denied); expected LUKS UUID {expected}\n"
                ),
            )
        );
    }

    // Intent: the inactive close-skip emitter renders the caller-owned inactive
    //   warning line exactly.
    // Why it exists: inactive probes are silent in the shared helper, so the
    //   sibling emitter is the single production source for that user-visible
    //   warning.
    // Scenario: a command reaches its post-commit close path after the mapper
    //   has already disappeared and warns that the close was skipped.
    #[test]
    fn warn_close_skipped_inactive_renders_expected_line() {
        let mapper = wrong();
        let expected = test_uuid(718);

        let output = crate::status_tag::testing::capture_with_color(false, || {
            warn_close_skipped_inactive(&mapper, &expected);
        });

        assert_eq!(
            output,
            format!(
                "Warning: post-commit close skipped for mapper {mapper}: probe failed (mapper is inactive); expected LUKS UUID {expected}\n"
            ),
            "inactive close-skip emitter must render the caller-owned warning"
        );
    }
}
