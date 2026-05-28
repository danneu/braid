# Plan: fail closed on untrusted missing-device allocation shape

## Summary

This is a follow-up to
`plans/impl/2026-05-28-btrfs-device-usage-missing-fixture.md`. That
implemented fixture plan caught and pinned the live missing-device output
shape; this plan addresses the remaining runtime behavior gap surfaced
by that work.

HEAD already handles the path marker correctly: production keys
`remove-missing` on devid and `device_size == 0`, not on
`<missing disk>`. The remaining runtime gap is allocation-row drift.
`parse_btrfs_device_usage` accepts a missing-device stanza with no
allocation rows, and `check_raid1_relocation_space` treats absent
per-type rows as zero bytes, so a gross runtime output change could make
`remove-missing` undercount relocation demand.

The ideal fix is to keep the parser permissive for read-only surfaces,
but make `remove-missing` enforce a stricter missing-device contract
before it calls the relocation preflight. This puts the hard safety
check at the mutating command that owns the degraded-pool crash risk,
without breaking status, doctor, TUI, or healthy remove behavior.

The guard validates the *shape* braid can trust, not per-type
completeness. btrfs RAID1 allocates chunk pairs to the two devices with
the most unallocated space, and the System chunk is a single tiny pair,
so a 3+ device pool member -- including one that goes missing -- may
legitimately hold only a subset of `{Data, Metadata, System}`. The
guard therefore rejects only what braid cannot reason about (an
unsupported profile, a non-unique target stanza, or a stanza with no
positive supported row at all) and lets a legitimately sparse RAID1
target through.

## Key Changes

- Add a private validation step inside `check_relocation_space`
  (`cli/src/remove_missing.rs`), after the existing `target.is_empty()`
  absent-target guard and before `preflight::check_raid1_relocation_space`.
- Reject duplicate target matches: if more than one usage stanza has
  `device_size == 0 && devid == missing_id`, refuse. Today the
  `target: Vec<_>` path would silently sum the duplicates through
  `check_raid1_relocation_space`.
- Trust the target's allocation shape rather than requiring all three
  types to be positive:
  - Reject if any positive (`bytes > 0`) allocation row on the target is
    outside the supported `Data|Metadata|System` x `RAID1` contract
    (e.g. `Data,single`, `Metadata,RAID1C3`, an unknown type or
    profile). `DeviceAllocation` carries `alloc_type`, `profile`, and
    `bytes`, so the check reasons per cell.
  - Require at least one positive supported `{Data|Metadata|System},RAID1`
    row. A target with no positive supported row (empty allocations,
    all-zero rows, or only unsupported profiles) is refused -- braid
    cannot distinguish "no relocation work" from "output drift hid the
    rows."
  - Treat an absent supported type as zero demand and let
    `check_raid1_relocation_space` skip it via its existing zero-type
    behavior. This is what lets a legitimately sparse missing device
    (e.g. a 3-disk pool member that never held a System chunk) pass.
- On rejection, return `RemoveMissingError::Validation` with a
  cause-specific message that matches the existing fail-closed wording
  (`... Refusing to remove the missing device without a validated
  relocation-space check. Inspect `btrfs device usage --raw {mount}`
  manually, then re-run.`):
  - duplicate stanza: names that devid is listed more than once.
  - unsupported profile: names the offending `{type},{profile} = {bytes}`
    cell.
  - no supported row: states the target has no positive Data/Metadata/System
    RAID1 allocation.
- Do not change `parse_btrfs_device_usage`: it should keep parsing
  unknown future rows permissively.
- Do not change generic `check_raid1_relocation_space`: healthy remove
  still needs its documented "skip zero type" behavior.
- Correct misleading comments/docs that attribute `<missing disk>` to a
  separate btrfs-progs loader. The accurate source is the Linux btrfs
  `BTRFS_IOC_DEV_INFO` path via `btrfs_dev_name()`; btrfs-progs copies
  that path.
- Document the new refusal in both user-facing and design surfaces:
  - `docs/commands/remove-missing.md` "Safety checks / refusal cases":
    extend the ENOSPC pre-flight bullet so it also lists "an untrusted
    missing-device allocation shape (the targeted devid is listed more
    than once, carries an allocation profile braid does not model, or
    reports no positive Data/Metadata/System RAID1 row)."
  - ADR 012 (`docs/design/decisions/012-intent-cli.md`) "ENOSPC
    pre-flight check" section: note that `remove-missing` also refuses
    an untrusted missing-device allocation shape before
    `btrfs device remove`, and that the trust check validates shape
    (supported RAID1 profiles, a unique target stanza, at least one
    positive supported row), not per-type completeness.

## Tests

- Keep the existing fail-closed tests unchanged (spawn error, nonzero
  exit, unparseable output, absent target). They already pin the
  fail-closed contract for the non-shape uncertainties.
- Replace `check_relocation_space_passes_present_zero_allocation_missing_target`:
  a present missing target with zero allocations now fails closed
  (no positive supported row).
- Add accept coverage for legitimately sparse supported shapes that the
  rejected all-three rule would have wrongly refused:
  - missing target with only `Data,RAID1` positive (no Metadata, no
    System row) passes when survivors have space.
  - missing target with `Data,RAID1` and `Metadata,RAID1` positive but
    no System row passes when survivors have space.
- Add reject coverage: a positive unsupported profile on the target
  (e.g. `Data,single` or `Metadata,RAID1C3`) fails closed; assert the
  message names the unsupported cell.
- Add reject coverage: a target whose only supported rows are present
  but zero-bytes (no positive supported row) fails closed.
- Add reject coverage for the duplicate target stanza: two
  `device_size == 0` stanzas for the same `missing_id` via
  `device_usage_raw_body(&[DeviceUsageSpec::missing(3, ...),
  DeviceUsageSpec::missing(3, ...)])`. Assert the duplicate-target
  validation error fires before any relocation-space math (the error is
  the duplicate message, not "not enough space to relocate").
- Keep existing `preflight::raid1_space_skips_zero_allocation_type`
  unchanged to prove the stricter rule is scoped to `remove-missing`,
  not the generic preflight helper.
- Run `just test-rust`.
- Run focused VM coverage for the real degraded path:
  `just test-vm braid-remove-missing-enospc braid-remove-missing-enospc-crash braid-remove-missing-preflight-fails-closed braid-remove-disk`.

## Assumptions

- Strictness applies to `remove-missing` only. Healthy `remove` with 2+
  survivors keeps its warn-and-proceed policy because btrfs can fail that
  path cleanly.
- A legitimately sparse RAID1 shape is valid and must pass. Because btrfs
  allocates RAID1 chunk pairs to the two most-free devices and keeps the
  System chunk as a single tiny pair, a 3+ device pool member -- including
  one that goes missing -- may hold only a subset of
  `{Data, Metadata, System}`. Requiring all three positive would refuse
  valid cleanup, so the guard checks shape (supported profiles, a unique
  stanza, at least one positive supported row), not completeness.
- A missing target with no positive supported RAID1 allocation row
  (empty, all-zero, or only unsupported profiles) is rejected. This may
  block a rare no-op missing-device case -- e.g. a device added but never
  balanced before it died -- but that is the right tradeoff: braid cannot
  distinguish "truly no relocation work" from "runtime output drift hid
  the rows."
- The supported runtime contract for this preflight is RAID1 only. If a
  future pool shape needs RAID1C3, single, DUP, or another profile on a
  missing target, that should be added as an explicit new preflight model,
  not accepted accidentally.
