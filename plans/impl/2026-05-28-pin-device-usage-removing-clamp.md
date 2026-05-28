# Pin the live-removal shape on `btrfs-device-usage-removing` and add a synthetic negative-`Unallocated` test

## Context

A code-review finding claimed `cli/tests/fixtures/nixos-25.11/btrfs-device-usage-removing.txt`
(and its `nixos-unstable` twin) was a captured fixture that no Rust test consumed --
dead weight from `tests/progress-monitoring.py:164`.

That headline is wrong. The fixture is read by `golden_btrfs_device_usage_removing`
in `cli/tests/support/golden_common.rs:454-464`, which is `include!`-d by both
`golden_nixos_25_11.rs` and `golden_nixos_unstable.rs`. The investigator missed it
because they grepped only `cli/src/parse/btrfs_device_usage.rs`, not the test harness.

However, the finding's *underlying* observation is real and worth salvaging: the
existing assertions are loose. They only require `!devices.is_empty()` and that some
device has `used_bytes() > 0`. Both would pass against `btrfs-device-usage-2disk.txt`
just as well, so the test does not actually pin anything specific to the in-progress
removal it was captured to represent.

The whole reason this fixture exists is to exercise the live-removal shape the parser
specifically works around:

- `cli/src/parse/btrfs_device_usage.rs:30-37` -- `parse_kv_line` uses `parse_i64` and
  clamps with `value.max(0) as u64` *only* because btrfs reports negative `Unallocated`
  during device removal. The comment at lines 28-29 says so explicitly.
- The captured fixture matches: `disk3` has `Device slack: 4278190080` (= `Device size`)
  and `Unallocated: -1375731712`. After the parser runs, `unallocated` is clamped to 0.
- No synthetic test in `btrfs_device_usage.rs` covers this clamp. The captured fixture
  is the only thing currently exercising it -- and only by the implicit "parser must
  succeed" assertion, not by anything stated.

The plan tightens the golden assertions so they actually pin the live-removal contract,
and adds a small synthetic test inline in the parser module so the clamp has direct
unit-test coverage that does not depend on a captured fixture being present.

## Scope

Two files change. No production code changes. No fixtures change.

### 1. Strengthen `golden_btrfs_device_usage_removing`

File: `cli/tests/support/golden_common.rs`, lines 454-464.

Replace the closure body with assertions that pin the in-progress-removal shape:

- The fixture parses successfully (already implicit via the `golden_test!` macro's
  `.expect()` -- keep it).
- There is at least one "removing" device, defined as
  `device_size > 0 && device_slack == device_size`. In btrfs-progs, slack reaches
  `Device size` when the kernel marks the entire device as unavailable for new
  allocations -- the canonical signature of an in-progress `btrfs device remove`.
  The `device_size > 0` guard is required because btrfs-progs renders missing
  devices with `Device size: 0` and `Device slack: 0`
  (`reference/btrfs-progs/cmds/filesystem-usage.c:436-441, 819-832`; mirrored by
  the missing-device fixture builder at `cli/src/test_fixtures/shared.rs:509`), so
  the unguarded `device_slack == device_size` form trivially matches the
  `0 == 0` degenerate case and a future degraded-capture would false-positive.
  This is the structure-insensitive way to identify the removing device without
  hard-coding `disk3` or devid 3.
- That removing device has `unallocated == 0`. The captured value was negative
  (`-1375731712`); seeing `0` here proves the `parse_i64` + `.max(0) as u64` clamp at
  `cli/src/parse/btrfs_device_usage.rs:30-37` actually ran. Without the clamp the
  parser would either fail (the previous `parse_u64` form) or overflow.
- That removing device has `used_bytes() > 0`. During a live remove btrfs is
  actively relocating block groups off of it, so allocations are still present;
  asserting they're non-zero proves the fixture genuinely captures the transient
  state and not a post-remove snapshot.
- There is also at least one non-removing "survivor" device, defined as
  `device_size > 0 && device_slack == 0`. The `device_size > 0` guard mirrors the
  removing-device predicate above: without it a missing device (`device_size == 0,
  device_slack == 0`) would trivially satisfy the survivor assertion too. This pins
  the dual nature of a RAID1 remove -- one device shedding allocations, others
  absorbing them. A future capture that accidentally grabbed only the removing side
  (or only post-remove / fully-missing devices) would fail this.

The assertions are deliberately structural -- no hard-coded paths, devids, or byte
counts -- so the test stays robust across nixpkgs bumps and across stable/unstable
fixture regeneration.

### 2. Add a synthetic negative-`Unallocated` test in the parser module

File: `cli/src/parse/btrfs_device_usage.rs`, inside the existing `#[cfg(test)] mod tests`
block (around line 290, near the other synthetic tests).

Add one test that feeds inline btrfs-progs-shaped output with `Unallocated: -1375731712`
and `Device slack` equal to `Device size`, and asserts:

- The parser returns `Ok`.
- `unallocated == 0` (clamped).
- `device_slack == device_size` (round-trips).
- Allocations are preserved.

Use the standard preamble (`// Intent: ...`, `// Why it exists: ...`, `// Scenario: ...`)
per the project's Test Conventions in `AGENTS.md`, citing
`cli/src/parse/btrfs_device_usage.rs:30-37` as the workaround being protected and
`tests/progress-monitoring.py:164` as the fixture-capture source the synthetic version
mirrors.

This is belt-and-suspenders: the strengthened golden test already covers this with the
real fixture, but the synthetic version means the clamp remains tested even when:

- Fixtures are skipped because `REQUIRE_FIXTURES` is `false` in the stable lane and a
  fixture happens to be absent locally (`cli/tests/support/golden_common.rs:8-9, 26-32`).
- A future refactor regresses `parse_kv_line` to `parse_u64` -- the synthetic test
  fails immediately under `just test-rust` without needing a VM fixture round-trip.

## Reused utilities and patterns

- `golden_test!` macro in `cli/tests/support/golden_common.rs:35-60` -- already wires
  the fixture-skip-or-fail logic for both lanes. Reuse as-is; only edit the closure body.
- Existing inline synthetic-test pattern in `cli/src/parse/btrfs_device_usage.rs:197-374`
  (e.g. `device_usage_single_device`, `device_usage_parses_missing_device_marker`).
  The new test mirrors their shape: build a `RawCommandOutput` with an inline `stdout`
  literal, call `parse_btrfs_device_usage`, assert on fields.
- Test preamble convention: see `device_usage_parses_missing_device_marker` at
  `cli/src/parse/btrfs_device_usage.rs:228-265` for an in-file example of the
  Intent/Why/Scenario block.
- `BtrfsDeviceUsageEntry` fields used by the new assertions are all already public
  (`cli/src/parse/types.rs:457-465`): `device_size`, `device_slack`, `unallocated`,
  `allocations`. No new accessors needed; `used_bytes()` is already there too.

## Out of scope

- The fixture itself stays untouched. Both lanes already have it. The capture step in
  `tests/progress-monitoring.py:155-166` (using `dm-delay` to keep the remove slow
  enough to observe) is correct and not the source of the problem.
- The parser implementation stays untouched. The `parse_i64` + clamp is correct and
  the comment at lines 28-29 already explains why.
- No changes to `golden_nixos_25_11.rs` or `golden_nixos_unstable.rs`; both pick up
  the strengthened golden via the existing `include!("support/golden_common.rs")`.
- No fixture regeneration. The strengthened assertions are designed to pass on the
  current `nixos-25.11` and `nixos-unstable` captures (which are byte-identical).

## Verification

1. `just test-rust` -- runs both the new synthetic test in
   `cli/src/parse/btrfs_device_usage.rs` and the strengthened golden test for both
   lanes (stable runs against `nixos-25.11`; unstable runs against `nixos-unstable`).
   Both should pass against the currently-captured fixtures.
2. Sanity-check the test actually pins the new contract: temporarily mutate
   `parse_kv_line` at `cli/src/parse/btrfs_device_usage.rs:35` from `parse_i64` back to
   `parse_u64` and re-run `just test-rust`. The synthetic test must fail with a parser
   error, and `golden_btrfs_device_usage_removing` must also fail. Revert the mutation.
   (Do this only as a verification step; do not commit it.)
3. No VM tests are required for this change -- it is test-code only. The capture step
   in `tests/progress-monitoring.py` is unchanged, so no `just test-vm progress-monitoring`
   rerun is needed.

## Implementation notes

- Both new tests cite the clamp by function name (`parse_kv_line`) instead of the
  line range `btrfs_device_usage.rs:30-37` the plan specified. The synthetic test
  lives in the same module as `parse_kv_line`, so a same-file line citation rots on
  any edit above it; the function name is stable and unambiguous. The cross-file
  `tests/progress-monitoring.py:164` citation in the synthetic preamble is kept as
  the plan specified.
- Verified the mutation sanity-check (plan Verification step 2): swapping `parse_i64`
  back to `parse_u64` fails both `device_usage_clamps_negative_unallocated` and
  `golden_btrfs_device_usage_removing` with `MissingField` on `Unallocated`,
  confirming neither assertion is vacuous. Reverted; not committed.
