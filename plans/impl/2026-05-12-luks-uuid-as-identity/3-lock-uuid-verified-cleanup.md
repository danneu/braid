# Plan: remove name-based `lock` identity fallback while preserving best-effort cleanup

## Summary

Change `braid lock` so `braid-*` mapper names are only cleanup candidates, never
proof of member identity. Keep `lock` best-effort by falling back from btrfs pool
probing to direct mapper probing, but require each mapper to be classified by
backing LUKS UUID before it is treated as member-owned.

The key rule: if `lock` cannot verify a mapper by LUKS UUID or btrfs devid, it
warns and skips that mapper instead of closing it based only on
`braid-<DiskName>`.

## Key Changes

- Replace the current `FsidOnly` behavior with a UUID-scanned fallback.
- Mounted pool with successful `probe_pool`: keep the current UUID/devid
  close-set path.
- Mounted pool with `probe_pool` failure: run the existing FSID/preflight proof,
  then scan `/dev/mapper/braid-*` candidates and classify each via
  `cryptsetup status` backing device plus `cryptsetup luksUUID`.
- Unmounted pool: scan `/dev/mapper/braid-*` candidates and classify each via
  the same backing-UUID path.
- Remove member-owned reconstruction from `mapper_name(&member.name)` in the
  fallback path.
- Classify fallback candidates as:
  - UUID matches membership: close observed mapper as `MemberOwned`.
  - UUID is readable but not in membership: close observed mapper as `Orphan`
    with the existing orphan warning.
  - UUID/backing device cannot be verified: do not close; add a warning such as
    `skipping mapper braid-x: cannot verify backing LUKS UUID (...)`.
- Have `LockPlan` carry skipped candidate mapper names separately from
  `LockCloseSet`. Skipped mappers must suppress the membership-side
  `already closed` prelude for the matching expected mapper name, but must not
  appear in forget paths, dry-run close steps, or real close calls.
- In the UUID-scanned fallback, `/dev/mapper` scan failure means warn and plan
  no fallback closes. The only exception is the successful full `probe_pool`
  path, where already-proven `pool.devices` and `pool.null_underlying` entries
  may still be closed even if the extra orphan scan fails.
- Have `LockPlan` carry a cleanup-uncertain flag when a candidate is skipped or
  `/dev/mapper` cannot be scanned in a fallback path. In real execution, this
  suppresses `pool already locked` when no close runs. In dry-run, add a
  `PreviewNote::Info` such as `cleanup incomplete: some braid mappers could not
  be verified` so warning-only previews do not render `nothing to do.`
- Route all planner warnings through `PreviewNote::Warn`, so dry-run and real
  execution show the same warnings and planning does not emit ad hoc stderr.
- Update ADR 024:
  - Remove the statement that `lock` may close `braid-<member-name>` under
    FSID-only fallback without UUID verification.
  - State that `lock` may use the `braid-*` prefix only to find cleanup
    candidates; member identity still requires UUID/devid, and unverified
    candidates are skipped.

## Tests

- Add or adjust `cli/src/lock.rs` unit tests:
  - Mounted `probe_pool` failure still plans useful cleanup when mapper backing
    UUID matches membership.
  - A mapper named like a member but backed by a different UUID is not
    classified as that member.
  - Unmounted `lock` closes a UUID-verified member mapper, including drifted
    observed mapper names.
  - Unverified `braid-*` candidates are warned and skipped, not closed as orphan
    by name.
  - A skipped member-named mapper such as `braid-disk1` suppresses
    `disk disk1: already closed` while still producing no forget path and no
    close step.
  - UUID-scanned fallback scan failure emits the scan warning and plans no
    fallback closes; update the old scan-failure expectation that relied on
    name-derived member closes.
  - `plan_lock(...).preview().render()` for an unverified `braid-*` candidate
    shows the skip warning before any steps and contains no close or forget step
    for the skipped mapper.
  - Command-level dry-run stream routing for an unverified `braid-*` candidate
    runs `braid lock --dry-run`, asserts the skip warning is on stdout, and
    asserts stderr is empty.
  - Warning-only uncertain cleanup does not render a clean no-op: dry-run must
    not include `nothing to do.`, and real execution must not print
    `pool already locked`.
  - Existing full-arm stranded mapper classification failure test should expect
    skip plus warning, not orphan close.
- Keep existing tests for:
  - Full-arm UUID/devid close-set behavior.
  - Observed mapper close/forget paths.
  - Orphan close for readable non-member UUIDs under the `braid-*` namespace.
  - NotBtrfs mounted preflight still aborts.
- Run:
  - `just test-rust`
  - Targeted VM tests: `luks-mapper-drift`, plus any existing `lock`/unlock
    recovery VM tests that exercise mapper drift or lock cleanup.

## Assumptions

- `braid-*` remains a reserved cleanup namespace, so readable non-member UUIDs
  under that prefix may still be closed as orphans.
- A mapper that cannot be UUID-verified is safer to leave open than to close by
  name.
- Best-effort `lock` means "do all cleanup that can be proven safe," not "close
  every plausible braid-looking mapper."
