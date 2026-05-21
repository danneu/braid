# Refactor: parameterize `cmd_idle` busy-exclop tests with short-circuit assertion

## Context

The autosuspend gate in `cmd_idle` (`cli/src/idle.rs:48-105`) does a
cheap sysfs scan before spawning the `btrfs scrub status` subprocess.
The ordering is operationally important: each spurious scrub probe
costs a fork/exec on the autosuspend timer, and the
fail-closed-on-Unknown contract means a scrub-side failure that was
already preempted by a busy sysfs exclop would silently turn a clean
short-circuit into `Busy::Unknown`.

The contract "kernel exclop wins over scrub probe" is only pinned for
`balance` -- by `busy_exclop_short_circuits_scrub_probe`
(`cli/src/idle.rs:279-290`), which is the one test asserting
`runner.requests().is_empty()` on the busy-exclop path. The other six
busy-exclop tests (`busy_when_balance_paused`, `busy_when_device_add`,
`busy_when_device_remove`, `busy_when_device_replace`,
`busy_when_resize`, `busy_when_swap_activate` at
`cli/src/idle.rs:310-367`) use `idle_ready_for_sysfs_check` which
seeds a *finished* scrub. A regression that pre-spawns the scrub probe
before the sysfs check would silently consume that seed and the tests
would still pass because the finished-scrub state still maps to
`IdleResult::Idle` -- which gets overridden by the subsequent busy
exclop, so the test's `BusyReason::Exclop(...)` assertion still holds.

The seven `busy_when_<op>` test bodies are also near-duplicates of
each other, three lines each that differ only in the exclop string
and the expected `ExclusiveOp` variant. Collapsing them into one
table-driven test closes the coverage gap and removes the duplication
in a single change.

## Approach

Replace the eight tests (`busy_exclop_short_circuits_scrub_probe` +
the seven `busy_when_<op>` tests) with one parameterized test that
iterates over the seven exclop cases and, for each, asserts both:

1. `cmd_idle` returns `IdleResult::Busy(BusyReason::Exclop(<variant>))`.
2. `runner.requests().is_empty()` -- proving no scrub probe spawned.

Use `MockRunner::default()` (no scrub seed) instead of the existing
seeded-but-unconsumed pattern. The reasoning: if the regression ever
pre-spawns a scrub probe, `MockRunner::default()` returns
`CmdError::MissingMock` (`cli/src/cmd.rs:1206`, 1431), which surfaces as
`Busy::Unknown` and fails the `BusyReason::Exclop(...)` assertion
loudly. This is a second observable failure mode beyond the explicit
`runner.requests().is_empty()` check -- belt-and-suspenders for the
fail-closed direction.

Follow the existing `for (input, expected) in [...]` table-driven
idiom used at `cli/src/doctor.rs:1909-1914` (and the in-file
`busy_reason_display_pins_cli_strings` at `cli/src/idle.rs:230-271`).
The seven `(string, variant)` pairs mirror the canonical mapping in
`ExclusiveOp::parse` at `cli/src/preflight.rs:89-101` -- list them
explicitly in the test rather than reusing `parse`, because the test
exercises `cmd_idle`'s response to a busy exclop, not the parser
itself.

## Sketch

```rust
// Intent: every kernel exclop string is reported as the matching
//   BusyReason::Exclop and short-circuits the scrub-status subprocess.
// Why it exists: pins two contracts at once. (1) Coverage for the
//   post-refactor exclop surface -- before the sysfs scan, only
//   `balance` / `balance paused` were detected and the other five were
//   silently reported as idle. (2) The sysfs-before-scrub ordering
//   matters operationally: each spurious scrub spawn is a fork/exec
//   on the autosuspend timer. Using MockRunner::default() (no scrub
//   seed) makes any regression that pre-spawns the scrub probe fail
//   loudly as Busy::Unknown via MissingMock, in addition to the
//   explicit `runner.requests().is_empty()` check.
// Scenario: Operator runs `btrfs device remove` directly on the pool;
//   `braid idle` must report busy without spending a subprocess on
//   `btrfs scrub status`.
#[test]
fn busy_exclop_short_circuits_scrub_probe() {
    let cases = [
        ("balance", ExclusiveOp::Balance),
        ("balance paused", ExclusiveOp::BalancePaused),
        ("device add", ExclusiveOp::DeviceAdd),
        ("device remove", ExclusiveOp::DeviceRemove),
        ("device replace", ExclusiveOp::DeviceReplace),
        ("resize", ExclusiveOp::Resize),
        ("swap activate", ExclusiveOp::SwapActivate),
    ];

    for (exclop, expected) in cases {
        let runner = MockRunner::default();
        let fs = IdleMockFs::with_exclop(exclop);

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::Exclop(expected)),
            "exclop={exclop:?}",
        );
        assert!(
            runner.requests().is_empty(),
            "exclop={exclop:?}, requests={:?}",
            runner.requests(),
        );
    }
}
```

The `exclop={exclop:?}` annotation on both asserts is critical:
without it, a failure on a single case in the loop is undiagnosable.

## Files to modify

- `cli/src/idle.rs` -- delete the old
  `busy_exclop_short_circuits_scrub_probe` (lines 273-290) and the
  seven `busy_when_<op>` tests (lines 292-367); add the parameterized
  replacement in the same location.
- `cli/src/idle.rs` -- the `busy_when_scrub_running` test
  (lines 204-222) has a "Pre-condition" comment naming
  `busy_exclop_short_circuits_scrub_probe` as the test that pins the
  sysfs-first ordering. The reference still resolves under the same
  name, so no edit is needed -- but verify the reference is intact
  after the rewrite.

No changes to `cli/src/test_fixtures/idle.rs`, `cli/src/preflight.rs`,
or `cli/src/cmd.rs`. `idle_ready_for_sysfs_check` and
`idle_runner_with_scrub_finished` remain used by other tests in the
same module (`idle_when_all_ops_quiet`,
`busy_unknown_on_sysfs_read_failure`,
`no_balance_or_replace_subprocess_calls`, the multi-fsid /
fail-closed-listing tests at lines 549-658), so they stay.

## What is NOT changed

- `busy_reason_display_pins_cli_strings` at `cli/src/idle.rs:230-271`
  is left alone. It tests the `Display` impl, not `cmd_idle` behavior;
  consolidating it would conflate two contracts.
- `busy_unknown_on_unrecognized_exclop` at `cli/src/idle.rs:375-380`
  is left alone. It covers the `Err` arm of `ExclusiveOp::parse`,
  which is structurally different from the seven recognized-variant
  cases.
- The "scrub probe IS reached after clean sysfs" path is already
  pinned by `no_balance_or_replace_subprocess_calls` at
  `cli/src/idle.rs:411-422` via `assert_eq!(runner.requests(), vec![BtrfsScrubStatus])`.
  This refactor does not touch that direction.

## Verification

1. `just test-rust` -- the parameterized test must pass. The seven
   variants exercise the canonical exclop strings from
   `ExclusiveOp::parse`.
2. Negative verification of the new assertion: temporarily reorder
   `cmd_idle` so the scrub probe runs unconditionally before the
   sysfs check, then `just test-rust`. Every case in the parameterized
   test must fail -- with `Busy::Unknown` (MissingMock) for the result
   assertion or a non-empty `runner.requests()` if a seeded scrub is
   reintroduced. Revert.
3. Negative verification of the loop annotation: temporarily corrupt
   one case (e.g. map `"device remove"` to `ExclusiveOp::DeviceAdd`),
   `just test-rust`, and confirm the failure message includes
   `exclop="device remove"` so the loop iteration is identifiable.
   Revert.
4. No VM tests required -- this refactor is pure test-side and does
   not touch product code or VM-level behavior.
