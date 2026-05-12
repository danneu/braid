//! Shared LUKS UUID re-probe at observed mapper paths.
//!
//! The `cryptsetup luksUUID` round-trip against a `/dev/mapper/<name>`
//! slot is the defense-in-depth seam used to prevent post-commit closes
//! of foreign disks: between the planning UUID classification and the
//! per-mapper `cryptsetup close`, an operator can manually open a
//! different disk under the same mapper name. Probing the mapper's
//! current LUKS UUID immediately before close lets each call site
//! demote the close to a logged-warning skip on mismatch.
//!
//! Today the same helper exists inline in `remove.rs` and `replace.rs`;
//! Phase 4 of the LUKS-UUID-as-identity migration lifts the body here
//! so the two existing callers and the upcoming recovery-side callers
//! (`recover.rs:2935`, addressed in Phase 4b) share a single source of
//! truth. The helper is logger-coupled by design -- every failure path
//! emits the operator-facing Warning text and returns `false` so the
//! caller proceeds to skip the close. Phase 4b will reuse this body.

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_cryptsetup_luks_uuid;
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
    let device = format!("/dev/mapper/{}", mapper.0);
    let probe = match runner.run(&CmdRequest::CryptsetupLuksUuid { device }) {
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
