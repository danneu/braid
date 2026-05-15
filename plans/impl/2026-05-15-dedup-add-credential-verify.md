# Plan: dedup credential verify in `build_add_credential_prelude`

## Context

`build_add_credential_prelude` (`cli/src/add.rs:1669-1719`) builds the
operator-visible passphrase prelude for `braid add` by appending two
ordered groups to `verify_targets`:

1. One target per live pool member (`pool.devices` loop at
   `cli/src/add.rs:1689-1702`).
2. One target per user-passed disk that probed as `PresentLuks` (the
   `extend` at `cli/src/add.rs:1703-1711`).

When the operator runs `braid add existing=... new=...` and `existing`
is already a pool member, `existing` appears in **both** groups. The
downstream consumer (`AddPlan::execute` at `cli/src/add.rs:872-906`)
iterates the combined list and calls `verify_credential_for_targets`
(`cli/src/credential_verify.rs:34-77`), which runs
`cryptsetup --test-passphrase` per target. Each invocation is a full
Argon2 round and emits one `[wait]/[ok]` row. So the operator pays
a duplicate Argon2 cost and sees a duplicate progress row for the
same physical disk.

The work plan itself already classifies the duplicate as
`AddLuksIdentity::BraidLabeledAlreadyInPool` and emits zero work
(`cli/src/add.rs:1862`). So the second verify is purely redundant and
not load-bearing on any correctness invariant.

Sibling commands already implement this dedup:

- `cli/src/replace.rs:492-509` filters the old-uuid disk out of the
  retained members before building verify targets.
- `cli/src/recover.rs:2180-2244` (`verify_recover_passphrase_for_add_replay`)
  uses a `BTreeSet<LuksUuid>` of live members and `continue`s on match.

`add` is the outlier. The fix unifies its behavior with the existing
pattern.

## Approach

Add a UUID-based skip to the `extend` filter_map. Skip a candidate
`PresentLuks { uuid, .. }` whose `uuid` matches any
`pool.devices[*].luks_uuid`.

A linear scan over `pool.devices` is correct here -- the list is
bounded by a small number of disks (typical NAS ~2-8), so the
`BTreeSet` indirection used in `recover.rs` is not needed.

### Edit

`cli/src/add.rs:1703-1711` -- the `verify_targets.extend(...)` block.

Before:

```rust
verify_targets.extend(input.probed.iter().enumerate().filter_map(|(i, probed)| {
    match &probed.state {
        ConfigDiskState::PresentLuks { .. } => Some(CredentialVerifyTarget {
            name: input.names[i].as_str().to_owned(),
            device: input.by_ids[i].as_str().to_owned(),
        }),
        ConfigDiskState::Absent | ConfigDiskState::PresentNotLuks => None,
    }
}));
```

After:

```rust
verify_targets.extend(input.probed.iter().enumerate().filter_map(|(i, probed)| {
    let ConfigDiskState::PresentLuks { uuid, .. } = &probed.state else {
        return None;
    };
    // Skip when this UUID is already covered by the pool-side loop above.
    // Sibling pattern: recover.rs:2199-2201, replace.rs:492-509.
    if input.pool.devices.iter().any(|d| d.luks_uuid == *uuid) {
        return None;
    }
    Some(CredentialVerifyTarget {
        name: input.names[i].as_str().to_owned(),
        device: input.by_ids[i].as_str().to_owned(),
    })
}));
```

Nothing else moves. `pool_target_count` semantics are preserved
(it's still `input.pool.devices.len()`), so the rejection-message
branch at `cli/src/add.rs:888` (`target_idx < pool_target_count`)
remains correct.

### Why no shared helper

The verify-issue investigation considered hoisting `live_member_uuids`
from `cli/src/recover.rs:1962-1967` into a shared module. Rejected
because:

- It has zero callers outside `recover.rs`.
- The `add` callsite needs a single `.any()` linear scan, not a set;
  introducing a `BTreeSet<LuksUuid>` for the `add` case would be
  abstraction without benefit at this disk count.
- Hoisting now would touch three commands' callsites for no behavioral
  win.

If a fourth callsite appears later, hoist then.

## Test

Behavioral, structure-insensitive: assert the operator-cost surface
(count of `CryptsetupTestPassphrase` invocations) rather than the
internal `verify_targets.len()`.

Mirror the existing `AddRecordingRunner` scaffolding
(`cli/src/add.rs:5742-5945`), which already supports multi-disk mock
state and exposes a `log()` of `CmdRequest`s.

### New test

`cli/src/add.rs` in `mod tests`:

```rust
/* Intent: when the operator passes a disk already in the pool
 * alongside a fresh disk, the credential prelude must not verify
 * the in-pool disk twice (once via the pool.devices loop, again via
 * the PresentLuks extend).
 * Why it exists: each verify target costs a full Argon2 round and
 * emits a [wait]/[ok] row. Duplicating burns ~1-2s of operator wait
 * and renders a confusing twin row over SSH.
 * Scenario: pool has disk1 already (PresentLuks, mapper open,
 * classified BraidLabeledAlreadyInPool); operator runs
 * `braid add disk1=... disk2=...` where disk2 probes PresentNotLuks.
 * Expected: exactly one CryptsetupTestPassphrase invocation against
 * disk1's underlying device.
 */
#[test]
fn cmd_add_mixed_already_in_pool_and_fresh_verifies_each_disk_once() {
    // AddRecordingRunner with `pool_mounted = true` so probe_pool
    // returns disk1 as a live member.
    // Drive cmd_add for
    //   disk1=/dev/disk/by-id/virtio-disk1 (already in pool)
    //   disk2=/dev/disk/by-id/virtio-disk2 (fresh, PresentNotLuks)
    // with `dry_run = false` so AddPlan::execute runs the credential
    // prelude (verify only runs in execute, never in dry-run preview).
    // Assert:
    //   runner.log().iter()
    //     .filter(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { .. }))
    //     .count() == 1
    //
    // The single CryptsetupTestPassphrase targets `/dev/vdb` (disk1's
    // underlying, via the pool.devices-loop entry); the candidate-side
    // entry for `/dev/disk/by-id/virtio-disk1` is now skipped because
    // its probed UUID matches the live pool member.
}
```

The test must drive `cmd_add` (not just `plan_add`), because the
prelude verify only runs in `AddPlan::execute`. The dry-run path
builds the prelude but never invokes `verify_credential_for_targets`
(`AddPlan::preview()` at `cli/src/add.rs:809-815` renders only notes
plus `work_plan.render_steps()`; per
`docs/decisions/022-dry-run-preview-model.md:50-52`, `Step` is
output-only and cannot expose the verify rows).

### Required mock extensions to `AddRecordingRunner`

The existing scaffolding (`cli/src/add.rs:5810-5950`) is set up so
that disks under test are probed as fresh (`PresentNotLuks`). It
returns `DISK1_UUID` only for `/dev/vdb` and the catch-all
`CryptsetupLuksUuid` branch (`cli/src/add.rs:5863-5871`) returns
"not a valid LUKS device" for every other device path. Three existing
bootstrap tests depend on that default:

- `cmd_add_bootstrap_aborts_on_passphrase_mismatch`
  (`cli/src/add.rs:6128-6182`)
- `cmd_add_bootstrap_proceeds_on_passphrase_match`
  (`cli/src/add.rs:6199-6238`)
- `cmd_add_with_keyfile_orders_format_addkey_backup_open`
  (`cli/src/add.rs:6314+`)

All three call `AddRecordingRunner::new(false)` with
`disk1=/dev/disk/by-id/virtio-disk1` and rely on the catch-all
to drop disk1 into `PresentNotLuks` so cmd_add reaches the
bootstrap-confirm + `luksFormat` chain. Adding the new mocks
unconditionally would reclassify disk1 as `PresentLuks` in those
tests, breaking them or silently changing what they exercise.

**Make the new behavior opt-in** via a builder method on
`AddRecordingRunner`, matching the existing pattern
(`with_backup_success`, `with_backup_failure_stderr` at
`cli/src/add.rs:5775-5782`). Name it something like
`with_disk1_present_luks_member()`. Only the new regression test
enables the flag; existing tests keep the default fresh-disk
behavior.

When the flag is set, the runner returns:

- `CryptsetupLuksUuid { device: "/dev/disk/by-id/virtio-disk1" }`
  -> `DISK1_UUID` (overrides the catch-all so disk1 reaches
  `PresentLuks` in `probe_config_disk` at `cli/src/probe.rs:177-191`).
- `CryptsetupLuksDumpText { device: "/dev/disk/by-id/virtio-disk1" }`
  -> a LUKS2 dump with `Label: braid-disk1` (required by
  `probe_config_disk` at `cli/src/probe.rs:204-214` for the
  LUKS2 invariant + label capture, then cross-checked by
  `validate_braid_preconditions` at `cli/src/add.rs:138-145`).
  Mirror the fixture style used by
  `probe_config_disk_present_luks_open` (`cli/src/probe.rs:829+`).
- `BtrfsFilesystemShowTarget { target: "/dev/mapper/braid-disk1" }`
  -> a btrfs show whose `uuid` equals `LIVE_POOL_FSID` (consumed by
  `classify_braid_disk_fsid` at `cli/src/add.rs:165-205` to return
  `BraidLabeledAlreadyInPool` rather than `BraidLabeledForeignPool`).

Unchanged regardless of the flag:

- `/dev/vdb` -> `DISK1_UUID` (`cli/src/add.rs:5855`), used by
  `probe_pool`'s per-member UUID lookup (`cli/src/probe.rs:467-470`).
- disk2 (`/dev/disk/by-id/virtio-disk2`) -> `PresentNotLuks` via
  the existing catch-all branch.

If the existing-mapper-open path needs additional support
(e.g. `classify_mapper_ownership` issuing extra requests inside
`probe_mapper_open` at `cli/src/probe.rs:247-261`), surface those
via the same opt-in flag or extend the test fixture analogously.

The real-run mutation chain past the prelude verify (luksFormat for
disk2, header backup, btrfs device add, mkfs, mount, journal) may
require additional mocks. Iterate on `CmdError::MissingMock` failures
and either reuse the post-format scaffolding already in
`AddRecordingRunner` (`with_backup_success`, etc.) or short-circuit
the test by asserting the verify count before a missing downstream
mock would abort (e.g. with the existing forced-backup-failure
behavior, which already lets cmd_add abort cleanly after the prelude
runs). Pick whichever yields the shortest test.

### Existing tests to not regress

- `plan_add_already_in_pool_is_note_only_success`
  (`cli/src/add.rs:7021-7070`) -- single-disk already-in-pool noop.
  After the fix this test still passes: no candidate verify target is
  added, only the pool-member one (no change in count).
- `verify_credential_for_targets_*` tests
  (`cli/src/credential_verify.rs:246+`) -- unchanged, the helper is
  not modified.

## Files

- `cli/src/add.rs` -- edit the `extend` in `build_add_credential_prelude`
  (lines 1703-1711); add one new test in `mod tests`.

## Verification

1. `just test-rust` -- new test passes, existing tests still pass.
2. Skim `cli/src/add.rs:1703-1711` post-edit -- confirm the skip
   references `input.pool.devices[*].luks_uuid` (not mapper name).

The unit test fully covers the regression; no manual NixOS-VM
sanity-check is needed. The dry-run preview path was deliberately
excluded as a verification surface: per
`docs/decisions/022-dry-run-preview-model.md:50-52`, `Step` is
output-only and the `[wait]/[ok]` credential rows are emitted from
`verify_credential_for_targets` (`cli/src/credential_verify.rs:46-77`)
inside `AddPlan::execute`, never from `AddPlan::preview()`.

## Out of scope

- Hoisting `live_member_uuids` to a shared module (see "Why no shared
  helper" above).
- Refactoring the four "build `CredentialVerifyTarget` from a
  `PoolDevice`" callsites (`add.rs:1689`, `replace.rs:510`,
  `recover.rs:2180`, `recover.rs:2921`) into a shared
  `CredentialVerifyTarget::from_pool_device` constructor. Tempting,
  but the body is two lines per callsite and the call shapes diverge
  on what to do with the candidate side. Park it.
