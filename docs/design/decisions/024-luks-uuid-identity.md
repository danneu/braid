---
intent: "Record the LUKS UUID Is Disk Identity decision and rationale. Read before changing related behavior or docs."
status: Active
---
> Active -- Refines [017-runtime-disk-membership.md](017-runtime-disk-membership.md).

# Decision: LUKS UUID Is Disk Identity


> Principle: [Stable identifiers](../principles.md#5-stable-identifiers)

## Context

Runtime membership originally used the operator disk name as the key in
`pool.json`. The same name also appears in mapper names and LUKS labels, so
code could accidentally treat display/runtime handles as identity. That made
label drift, mapper drift, and cloned disks hard to reason about: a member
could be the same encrypted device while its label or mapper path changed, or
two different by-id paths could expose the same cloned LUKS header.

## Decision

Use the LUKS UUID as the persistent disk identity. `pool.json` and
`pending-op.json` membership snapshots are keyed by canonical LUKS UUIDs.
`DiskMember.name` remains the operator-facing name and `DiskMember.by_id`
remains the hardware address used to reach the device. `DiskMember.devid` is
persisted only as prior-binding state for btrfs cases where the live device is
observable by devid but not by LUKS UUID, such as `null_underlying` mappers and
`missing_devids`.

Fresh `add` and `replace` operations pre-generate the UUID that cryptsetup must
write, store that UUID in the journal before mutation, and pass it through the
structured `CryptsetupLuksFormat` request. User-supplied `--luks-format-arg`
values may not override `--uuid` or `--label`.

## Identity Boundaries

| Identifier | Role | Persistent identity? | Normal user vocabulary? |
| --- | --- | --- | --- |
| `LuksUuid` | Encrypted-volume identity used for membership correlation, journals, duplicate detection, and live probe checks. | Yes | No |
| `DiskName` | Operator-facing name used in commands, status summaries, mapper suffixes, and labels. | No | Yes |
| `ByIdPath` | Hardware address used to find, open, or format a disk before it is mapped. | No | Setup and repair only |
| `DiskMember.devid` | Prior btrfs binding used when btrfs can report a device by devid but no live LUKS UUID is observable. | Fallback binding only | Repair diagnostics only |
| `braid-<DiskName>` mapper name | Runtime handle passed to cryptsetup, btrfs, mount, and close operations. | No | Mostly hidden |
| `braid-<DiskName>` LUKS label | Human/debug label for LUKS headers and discovery bootstrapping. | No | Mostly hidden |

This means UUID identity does not move normal command vocabulary from names to
UUIDs. Operators still add, replace, remove, and read disks by names such as
`toshiba1`. UUIDs belong in `pool.json`, journals, machine-readable status, and
diagnostics where braid must prove that the encrypted member is the expected
one.

## Benefits

- **Single source of truth.** `pool.json` has one persistent member identity:
  the LUKS UUID map key. Disk name, by-id path, and btrfs devid no longer
  duplicate or compete with a value-side `luks_uuid` field.
- **Drift-tolerant member correlation.** Commands resolve membership by UUID
  instead of reconstructing identity from `braid-<name>`. A member opened under
  a drifted mapper can still be recognized as the same disk, and cleanup paths
  close the observed mapper rather than the expected one.
- **Safer recovery replay.** Journals carry UUID-keyed pre-operation and target
  membership snapshots. Recovery can compare the live pool against the
  journaled member set by UUID/devid and re-check live UUIDs before replaying
  format, add, replace, resize, or close steps.
- **Earlier clone and swap detection.** Duplicate LUKS UUIDs are rejected before
  membership writes or destructive operations, and UUID mismatches catch disks
  that were swapped, cloned, or reformatted after the original plan was made.
  `add` and `replace` also re-probe the mounted pool at execution time before
  writing the journal, so confirmation/passphrase-window races still hit the
  UUID guard.
- **Human-facing names stay human-facing.** Operators still type and read disk
  names such as `toshiba1`; mapper names and labels remain `braid-<DiskName>`.
  UUIDs appear where they help diagnostics or machine-readable state, not as the
  normal command vocabulary.
- **Present-device probes use live paths.** Queries such as lsblk model/serial
  and smartctl use the live backing path (`PoolState::underlying_for_uuid`), and
  the TUI disk-detail LUKS metadata dump (`cryptsetup luksDump`) reads the live
  backing path for a verified-present (`Unlocked`) member -- not persisted by-id
  setup/repair handles that can drift while the disk is still present. Metadata
  for locked or ownership-unverified mappers stays on the by-id handle.

## Concrete Improvements

- **Membership shape is simpler.** Membership has one identity axis: UUID keys
  map to name/by-id/devid metadata.
- **Formatting is crash-replayable.** Fresh `add` and `replace` paths generate
  the UUID before mutation, journal it, and pass it to cryptsetup. Recovery can
  tell whether it is seeing the exact LUKS container that the interrupted plan
  intended to create.
- **Cleanup follows observed ownership.** `lock` classifies live mappers by
  UUID/devid and closes the mapper it actually observed. A mapper opened as
  `braid-WRONG` but owned by `disk1` is closed as `braid-WRONG`; braid does not
  merely try `braid-disk1` and leave the real mapper open.
- **Recovery compares member sets by identity.** Pending operations carry
  UUID-keyed pre-operation and target membership snapshots, so recovery can
  compare live topology with the journaled member set instead of re-discovering
  by label or assuming names still line up.
- **Display code has an explicit join rule.** User-facing summaries resolve a
  live pool device's UUID back to `DiskName` for presentation. UUIDs remain
  available to verbose/machine-readable paths where they are useful evidence.
  Every display surface uses the same join, including the TUI Data-tab Bus
  column (its lsblk transport bridge joins the parent disk's LUKS UUID to the
  member name) and the passphrase credential-verification display for `add`,
  `replace`, and their recovery replays, which resolves each existing pool
  member's name through the same live-UUID->`DiskName` join. Member names
  therefore survive mapper drift -- a member open as `braid-WRONG` still shows
  its operator name in the `passphrase: checking against ...` line and the
  `... does not match existing pool member '...'` rejection -- instead of
  blanking to `--` or echoing the drifted mapper basename.

## Runtime Handles And Labels

1. Mapper names remain `braid-<DiskName>`.
2. LUKS labels remain `braid-<DiskName>`.
3. Both mapper names and labels are presentation/runtime handles, not identity.
4. `LuksUuid` is the only persistent identity for membership decisions.
5. Code may construct `mapper_name(&member.name)` when opening or addressing
   braid's expected mapper.
6. Code must not parse mapper names or LUKS labels to decide membership, target
   a member, or correlate live pool state. Narrow exceptions are allowed for
   bootstrapping and sanity checks only: `discover` bootstraps from cold
   braid-labeled disks; returning-disk adoption in `add` may gate on label match
   after identity correlation still uses `LuksUuid`/`devid`/FSID; fresh add and
   replace recovery may require the expected label before treating an
   already-formatted target as the crash-created LUKS container, but still
   requires the journaled UUID to match. `lock` may use the `braid-*` prefix
   only to discover cleanup candidates; member identity still requires
   UUID/devid evidence, and candidates whose backing LUKS UUID cannot be
   verified are warned and skipped.
7. `lock` is the special cleanup case: classify live mappers by UUID/devid
   first, then close the observed mapper name, not a reconstructed
   `mapper_name(&member.name)`, so drifted-but-member-owned mappers are closed
   correctly. If mounted per-device probing fails, `lock` reads the mounted
   filesystem FSID to key the exclusive-operation preflight (so it will not
   unmount mid balance/replace), then scans `/dev/mapper/braid-*` candidates
   and closes only those with verified backing LUKS UUIDs. The unmount is
   licensed by mount-point ownership, not an FSID identity match (see Limits
   And Non-Goals). If a `null_underlying` mapper's persisted devid resolves to
   multiple membership UUIDs, `lock` warns, leaves that mapper open, and marks
   cleanup uncertain instead of demoting it to orphan cleanup. `lock` reports
   `disk <name>: already closed` only for members the planner has proved absent
   from every observed live state; it must not reconstruct
   `mapper_name(&member.name)` during execute to infer absence. If a mapper is
   skipped because classification failed, or `/dev/mapper` cannot be
   enumerated in either close-set arm, cleanup is uncertain and lock suppresses
   all already-closed claims for unobserved members.
8. Commands that reuse an already-open expected mapper for a requested by-id
   path must verify the mapper's canonical backing path before trusting the
   mapper's LUKS UUID. A cloned LUKS header can give two physical devices the
   same UUID, so the runtime proof is backing path match first, then UUID
   match.
9. Recovery must fail closed when a live btrfs device lacks an observable LUKS
   UUID and the journal has no persisted devid binding. It must not recover by
   inferring identity from `braid-<DiskName>`.
10. `replace` must re-probe the mounted pool after confirmation and passphrase
    verification but before sleep inhibitor acquisition, journal write, or
    `btrfs replace start`. If the pool is no longer mounted, the FSID differs
    from the planned pool, or any live pool device has the replacement target's
    LUKS UUID, replace fails closed with the canonical pre-journal validation
    or `DuplicateUuid { scope: LivePool }` refusal.

## Offline Disk State

A recorded member whose by-id path is present, whose LUKS header is readable,
and whose on-disk LUKS UUID matches the `pool.json` membership key is identity
verified. If that member is not assembled into the live btrfs pool, status and
TUI surfaces render it as `offline`, distinct from `missing` (device absent) and
`unknown` (braid cannot classify the state).

`offline` is deliberately cause-neutral. It can describe a locked member in a
degraded mount, an interrupted post-commit mutation, or another state where
membership and live btrfs topology have not yet been reconciled. Because those
causes have different remedies, `braid status` does not print an `Action:` hint
for offline rows.

`braid doctor`'s `declared_disks` check also surfaces an offline member as a
cause-neutral `Warn`, never `Fail`; `Fail` stays reserved for a live LUKS UUID
mismatch. When the pool is mounted but live topology cannot be probed, doctor
warns rather than claiming every declared member is assembled.

## Limits And Non-Goals

- A LUKS UUID identifies an encrypted LUKS container, not a physical drive,
  enclosure slot, SATA port, or by-id path.
- A cloned LUKS header intentionally has the same UUID as its source. Braid
  treats that as a duplicate identity and rejects it; it does not invent a new
  member identity for the clone.
- Mapper and label drift are tolerated for correlation and cleanup, but braid
  does not silently rewrite drifted mapper names or labels back into the
  expected `braid-<DiskName>` form.
- `devid` remains btrfs state. It is allowed only as a prior binding for
  missing/null-underlying cases where btrfs can still identify a member but
  braid cannot currently observe the LUKS UUID.
- A member with neither observable LUKS UUID nor journaled/persisted devid is
  not recoverable by mapper-name inference. The right behavior is to preserve
  recovery state and require manual reconciliation.
- UUIDs are not a user-facing naming scheme. They may appear in diagnostics,
  `pool.json`, `pending-op.json`, and machine-readable output, but command
  selection and normal summaries should continue to use `DiskName`.
- `lock`'s mounted-fallback teardown unmounts the configured btrfs mount point
  (licensed by mount-point ownership, not an FSID identity match -- braid
  persists no durable pool FSID to compare a probe against), then scans only
  `/dev/mapper/braid-*` and closes by backing LUKS UUID: verified member UUIDs
  close as members, verified non-member `braid-*` mappers close as orphans;
  non-`braid-*` devices and unverified candidates are skipped. The cleanup is
  scoped by the `braid-*` namespace plus UUID, not by which devices backed the
  unmounted filesystem. Consequence: a foreign btrfs at braid's mount point
  would be unmounted (a non-destructive, EBUSY-safe `umount` with no `-f`/`-l`);
  a foreign filesystem normally sits on non-`braid-*` devices, so the realistic
  consequence is the unmount alone. This is accepted, and gating it would
  require a durable pool-FSID identity axis this decision deliberately omits to
  keep membership single-axis.

## Tests That Enforce This

- `cli/src/membership.rs` unit tests pin UUID-keyed `pool.json`, reject stale
  value-side `luks_uuid`, and enforce duplicate checks across UUID, name,
  by-id, and devid axes.
- `cli/src/types.rs` and `cli/src/cmd.rs` unit tests reject user-supplied
  `--uuid`/`--label` extras and pin the structured
  `cryptsetup luksFormat --uuid <uuid> --label <label>` argv order.
- `cli/src/status.rs` unit tests pin compact status names by resolving live
  pool UUIDs back to `DiskName`, including a drifted mapper case.
- `cli/src/status.rs` and `cli/src/tui/probe.rs` unit tests pin that a present,
  LUKS-identity-verified member absent from the live pool renders `offline`, not
  `missing` or `unknown`.
- `cli/src/doctor.rs` unit tests pin that `declared_disks` renders verified
  members absent from the live pool as cause-neutral `Warn`, keeps UUID
  mismatches as `Fail`, preserves offline-pool identity-only behavior, and warns
  when mounted-pool topology cannot be probed.
- `tests/cli/braid-status-rust.py` pins that present disks' rendered
  `luks_uuid` equals the real cryptsetup UUID and the `pool.json` membership
  key, and that `name` is the operator name, in intact and degraded states.
- `tests/cli/braid-status-rust.py` pins that a degraded mount with one closed
  verified member renders that member as `OFFLINE` in human output and
  `offline` in JSON while the pool summary remains degraded.
- `tests/cli/braid-doctor-offline-member.py` pins that a degraded mounted pool
  with one closed verified member makes `declared_disks` warn with offline
  wording, while a fully assembled pool and an offline pool remain Ok.
- `tests/cli/status-mapper-drift.py` pins that `braid status` resolves the
  operator name via the UUID join when a member is open under a drifted mapper
  (`braid-WRONG`), not the mapper basename, in both JSON and human output.
- `cli/src/membership.rs`, `cli/src/add.rs`, `cli/src/replace.rs`, and
  `cli/src/recover.rs` unit tests pin that the passphrase credential-verify
  display resolves each existing pool member's name through the UUID join: a
  member open under a drifted mapper (`braid-WRONG`) is named by its operator
  name (`disk1`) in the verify prelude and the `... does not match existing pool
  member '...'` / `... rejected by '...'` rejection, not the mapper basename.
- `CredentialVerifyTarget`'s fields are private; its only constructors are
  `existing_pool_member` (display name UUID-joined through membership) and
  `named_candidate` (an operator-attested, already-validated `DiskName`), so
  no call site can put a mapper-derived name into a credential-verify target.
  `cli/src/credential_verify.rs` unit tests pin the drifted-mapper join and
  the foreign-UUID full-basename fallback at the constructor.
- `cli/src/tui/probe.rs` unit tests pin the TUI Data-tab Bus column's transport
  join to the parent disk's LUKS UUID, so a member open under a drifted mapper
  (`braid-WRONG`) still renders its bus instead of degrading to `--`.
- `cli/src/tui/probe.rs` unit tests pin that the disk-detail LUKS metadata dump
  reads the live backing path for a verified-present member (surviving by-id
  drift), and that a foreign / ownership-unverified mapper does not surface the
  live device's metadata under the declared disk.
- `cli/src/tui/app.rs` unit tests pin that session temperature watermarks
  accumulate by LUKS UUID: a member's watermark keeps widening across a
  disk-name change between probes, and a reused display name carrying a
  different LUKS UUID (`DiskName` is non-identity and reusable) starts a
  separate watermark instead of merging two disks' thermal history.
  `cli/src/tui/probe.rs` pins the producer side -- the reading's `id` is the
  member's live LUKS UUID.
- `cli/src/tui/probe.rs` and `cli/src/tui/browse/state.rs` unit tests pin that
  the TUI Browse SMART picker resolves a verified-present member through its live
  backing path (`PoolState.disk_underlying`, shared with the Data-tab SMART
  loop) and an offline member through its persisted by-id handle, so the two
  SMART surfaces cannot disagree under by-id drift. `tests/cli/braid-tui-browse.py`
  pins the live `/dev/vd*` node end-to-end for a present, unlocked member.
- `cli/src/lock.rs` unit tests pin the normal UUID/devid-classified close set,
  observed-mapper closing, UUID-scanned fallback cleanup, orphan warnings for
  non-member UUID/devid cases, duplicate-devid `null_underlying` skip behavior,
  and skip warnings for unverified candidates.
- `cli/src/remove.rs` unit tests pin all live member devids into the
  pre-operation journal snapshot before mutation, so recovery has a legitimate
  fallback binding when LUKS UUID is not observable.
- `cli/src/recover.rs` unit tests verify recovery refuses a null-underlying
  member when the journal lacks both observable UUID and persisted devid,
  instead of falling back to mapper-name inference.
- `cli/src/enroll_key_file.rs` unit tests verify standalone enroll rejects a
  member whose live LUKS UUID does not match the pool.json membership key
  before any slot inventory or keyfile mutation runs. Enroll also re-probes
  each member's live UUID again at its mutation boundary, after the passphrase
  prompt and before `luksAddKey`, to catch a disk swapped or reformatted during
  the prompt window: unit tests pin the standalone re-probe's mismatch and
  fail-closed arms and the discovery->execute window closure (a swap that passes
  discovery is rejected at execute before any keyfile is enrolled or generated).
- `cli/src/replace.rs` unit tests verify `ReplacePlan::execute` re-probes the
  live pool before journal write, rejects unmounted/FSID-drifted/colliding
  live-pool state, and still proceeds when the fresh probe is clean.
- `cli/src/recover.rs` unit tests verify post-maintenance replace recovery
  re-probes the old mapper UUID before close, skips foreign mappers, and still
  closes owned active dm mappings without relying on `/dev/mapper` path nodes.
- `cli/src/luks.rs` and `cli/src/probe.rs` unit tests verify already-open
  expected mappers must have the requested backing path before UUID ownership
  is accepted.
- `tests/cli/luks-mapper-drift.py` verifies `braid lock` closes the observed
  drifted mapper owned by a member UUID.
- `tests/cli/luks-lock-skipped-no-false-closed.py` verifies skipped mapper
  uncertainty does not produce false `already closed` rows.
- `tests/cli/unlock-uuid-mismatch.py`,
  `tests/cli/enroll-uuid-mismatch.py`, and
  `tests/cli/recover-replace-existing-luks-uuid-mismatch.py` verify swapped or
  reformatted disks fail UUID re-checks before unsafe replay, slot enrollment,
  or mount.
- `tests/cli/replace-new-in-pool-guard.py` verifies duplicate LUKS UUIDs are
  rejected before braid writes membership or calls into btrfs mutation.
- `tests/cli/replace-live-pool-collision-race-rejected.py` verifies replace's
  execute-time live-pool re-probe rejects a cloned replacement UUID added to
  the mounted pool while replace waits for confirmation.
- `tests/cli/braid-add-cloned-luks-header-rejected.py` and
  `tests/cli/replace-cloned-luks-header-rejected.py` verify cloned LUKS
  headers cannot make add or replace reuse a mapper opened from the wrong
  physical device.
- `tests/cli/braid-add-persists-before-balance.py` verifies fresh add writes
  canonical UUID-keyed membership, without a duplicate value-side `luks_uuid`,
  before post-add maintenance continues.
- `tests/cli/braid-doctor-uuid-swap.py` verifies `braid doctor` fails closed
  when a member's live LUKS UUID diverges from its pool.json key, surfacing
  the swap before any mutating command runs.

## Consequences

- `pool.json` key order is UUID order, not disk-name order. Display surfaces
  that need stable operator ordering must sort by `DiskName`.
- Recovery trusts journaled UUID-keyed membership snapshots for phase-specific
  replay and verifies live UUIDs again at mutation boundaries where a physical
  disk could have been swapped or reformatted.
- Mapper and label drift no longer break membership correlation, but drifted
  handles are not silently reconciled back into membership.
- Cloned disks with duplicate LUKS UUIDs are rejected before membership is
  written.

## Rejected Alternatives

1. **Keep disk name as identity.** Disk names are useful for humans but are not
   intrinsic to the encrypted device. Keeping them as identity preserves the
   label/mapper drift hazard.
2. **Use by-id as identity.** by-id paths identify hardware slots/devices, not
   encrypted membership. They can change with enclosures or controller
   behavior, and they do not detect cloned LUKS headers.
3. **Use btrfs devid as identity.** Devids are live filesystem state and are
   unavailable before mount. They remain useful only as fallback binding for
   missing or null-underlying devices.

## See

- [017-runtime-disk-membership.md](017-runtime-disk-membership.md)
- [../principles.md](../principles.md)
- `cli/src/membership.rs`
- `cli/src/journal.rs`
- `cli/src/recover.rs`
- `cli/src/lock.rs`
