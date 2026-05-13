# Decision: LUKS UUID Is Disk Identity

Status: Active -- Refines [017-runtime-disk-membership.md](017-runtime-disk-membership.md).

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

## Runtime Handles And Labels

1. Mapper names remain `braid-<DiskName>`.
2. LUKS labels remain `braid-<DiskName>`.
3. Both mapper names and labels are presentation/runtime handles, not identity.
4. `LuksUuid` is the only persistent identity for membership decisions.
5. Code may construct `mapper_name(&member.name)` when opening or addressing
   braid's expected mapper.
6. Code must not parse mapper names or LUKS labels to decide membership, target
   a member, or correlate live pool state. Two narrow exceptions are allowed:
   `discover` bootstrapping from cold disks, and returning-disk adoption safety
   in `add`, where the `PresentLuks` path may gate adoption on label match but
   identity correlation still uses `LuksUuid`/`devid`/FSID.
7. `lock` is the special cleanup case: classify live mappers by UUID/devid
   first, then close the observed mapper name, not a reconstructed
   `mapper_name(&member.name)`, so drifted-but-member-owned mappers are closed
   correctly.

## Consequences

- Old name-keyed `pool.json` and old journal shapes are rejected rather than
  migrated. Braid is unreleased, so operators cut over by rebuilding membership
  with `braid discover --write`.
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
