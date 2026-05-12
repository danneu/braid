# Reverse `braid idle` Probe Order

## Summary

Change `cmd_idle` so the cheap sysfs exclusive-op scan runs before the
`btrfs scrub status --raw` subprocess. This preserves fail-closed autosuspend
behavior while avoiding a scrub subprocess on every autosuspend tick during
long-running balance/add/remove/replace/resize/swap operations.

## Implementation Changes

- In `cli/src/idle.rs`, keep the mountinfo check first.
- Immediately after confirming the pool is mounted, call
  `check_any_btrfs_exclusive_op(fs)`.
- On `Ok(())`, continue to scrub probing.
- On `Err(ExclusiveOpError::Busy(op))`, return
  `IdleResult::Busy(busy_from_exclop(op))` without calling the runner.
- On `Err(Read | Unrecognized)`, return `BusyReason::Unknown(...)` without
  calling the runner.
- Move the existing `BtrfsScrubStatus` subprocess block after the sysfs scan.
- If scrub is running, return `BusyReason::ScrubRunning { pct }`.
- If scrub is finished or not running, return `Idle`.
- If the scrub command or parser fails, keep returning `BusyReason::Unknown(...)`.
- When scrub and a sysfs exclusive op overlap, sysfs wins because it is checked
  first.
- Update comments to describe the new order: sysfs first because it is cheap
  and catches kernel exclusive ops; scrub second because scrub is outside the
  kernel exclop set.
- Update stale test fixture comments that currently say strictness proves scrub
  short-circuits before sysfs.

## Public Interfaces

No public API, enum, exit-code, or NixOS option changes. CLI output can change
only in the overlap case where scrub and a sysfs exclusive op are both active:
`braid idle` now reports the sysfs-derived `busy:` reason instead of
`scrub running`.

## Test Plan

- Update `busy_when_scrub_running` to seed sysfs as clean, e.g.
  `IdleMockFs::with_exclop("none")`, so scrub-running behavior is still tested
  after the new sysfs-first gate.
- Add a new unit test in `cli/src/idle.rs`:
  - Seed `MockRunner` with `idle_scrub_running(...)`.
  - Seed `IdleMockFs::with_exclop("balance")`.
  - Assert `cmd_idle(...) == IdleResult::Busy(BusyReason::Balance)`.
  - Assert `runner.requests().is_empty()` to prove scrub status was not
    consulted.
- Keep `busy_unknown_on_scrub_probe_failure` as coverage that scrub is still
  called when sysfs is clean.
- Run `just test-rust`.

## Assumptions

- The desired semantics remain conservative: any sysfs exclusive op on any
  btrfs filesystem blocks suspend before scrub status is checked.
- No docs update is required because `docs/decisions/016-auto-suspend.md`
  already describes the sysfs scan as the post-mount exclusive-op gate.
