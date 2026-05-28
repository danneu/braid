# Plan: pin Fail-dominance for UUID mismatch + warn-level disk problem

## Context

`summarize_declared_disks` in `cli/src/doctor.rs:386-497` aggregates per-disk
`DiskState` classifications into a single `CheckResult`. The severity rule at
`cli/src/doctor.rs:491-495` is:

```rust
if uuid_mismatch.is_empty() {
    CheckResult::warn("declared_disks", message)
} else {
    CheckResult::fail("declared_disks", message)
}
```

The intent (per ADR-024, `docs/design/decisions/024-luks-uuid-identity.md:222-224`)
is that `braid doctor` must fail closed whenever a member's live LUKS UUID diverges
from its `pool.json` key -- this is the read-only early warning that surfaces
swapped/cloned/reformatted disks before any mutating command runs.

The existing test `summarize_declared_disks_promotes_to_fail_on_uuid_mismatch`
(`cli/src/doctor.rs:3319-3351`) only pairs `LuksUuidMismatch` with `LuksHeaderOk`.
With a healthy sibling, the rule "Fail iff `uuid_mismatch` is non-empty" and the
hypothetical regression "Fail iff `uuid_mismatch` is the *sole* non-empty problem
category" produce the same result, so a regression to the latter would slip past
every current test -- silently downgrading the ADR-024 fail-closed surface
precisely in the multi-fault scenario operators hit in practice (degraded pool +
swapped disk).

The other pure-summarizer tests (`summarize_mixed_states_reports_all`,
`summarize_preserves_missing_and_not_block_messages`) cover multi-fault combinations
but none of them include a `UuidMismatch` input.

This plan closes that gap with one new behavioral test.

## Change

Add a single `#[test]` inside the pure-summarizer test block in
`cli/src/doctor.rs` (the block starting at line 3099, immediately after
`summarize_declared_disks_promotes_to_fail_on_uuid_mismatch` at line 3351). The
test pairs a `LuksUuidMismatch` with a `LuksHeaderDamaged` input and asserts
`CheckStatus::Fail`, pinning the dominance rule against the specific regression
the existing coverage cannot catch.

## File to modify

- `cli/src/doctor.rs` -- one new test in the existing `#[cfg(test)] mod tests`
  block. No production code changes. No new helpers; reuses `cls` and
  `test_uuid`, which are already imported (see the `use crate::test_fixtures::{
  ..., cls, ..., test_uuid, ... }` block at lines 1673-1685).

## Test sketch

```rust
// Intent: when a UUID mismatch coexists with any other warn-level problem
//   (here: a damaged LUKS header), the check still reports Fail -- the
//   UUID-mismatch fail-closed posture from ADR-024 must dominate other
//   non-mismatch problem categories.
// Why it exists: the rule at lines 491-495 is "Fail iff uuid_mismatch is
//   non-empty." The existing uuid_mismatch test pairs the mismatch with a
//   healthy disk, so a regression to "Fail iff mismatch is the *sole*
//   problem category" would still pass. This test pins the actual rule.
// Scenario: a degraded NAS where one declared disk has been swapped
//   (UUID mismatch) and another has a damaged LUKS header -- the
//   exact multi-fault state ADR-024 fail-closed must catch before any
//   mutating command runs.
#[test]
fn summarize_declared_disks_fail_dominates_warn_level_problems() {
    let expected = test_uuid(1);
    let observed = test_uuid(2);
    let inputs = [
        cls(
            "disk1",
            "/dev/disk/by-id/wwn-0x1",
            DiskState::LuksUuidMismatch {
                expected: expected.clone(),
                observed: observed.clone(),
            },
        ),
        cls(
            "disk2",
            "/dev/disk/by-id/wwn-0x2",
            DiskState::LuksHeaderDamaged,
        ),
    ];

    let result = summarize_declared_disks(&inputs);

    assert_eq!(result.status, CheckStatus::Fail);
    // Sanity: both disks must appear so the test cannot be passed
    // by accidentally rendering only the uuid_mismatch input.
    let msg = &result.message;
    assert!(msg.contains("disk1"), "missing disk1: {msg}");
    assert!(msg.contains("disk2"), "missing disk2: {msg}");
}
```

Choice of second input: `LuksHeaderDamaged` matches the finding's primary
suggestion and represents a warn-level problem with its own non-empty bucket
(`header_damaged`), which is the exact distinction a regression to "sole
problem" semantics would key off. `Missing` would work too, but damaged is the
more on-point degraded-pool scenario for ADR-024.

## What is intentionally not changed

- No production code changes -- the rule at `cli/src/doctor.rs:491-495` is
  already correct.
- No new test helpers. `cls` (`cli/src/test_fixtures/doctor.rs:602`) and
  `test_uuid` (`cli/src/test_fixtures/shared.rs:30`) already exist and are
  already imported by the doctor test module.
- No augmentation of the existing `summarize_mixed_states_reports_all` or
  `summarize_declared_disks_promotes_to_fail_on_uuid_mismatch` tests. Mixing
  the new dominance assertion into either would broaden their intent
  ("renders all disk names" / "renders both sides of identity comparison")
  away from a single behavior, making future failures harder to localize.
- No table-driven cross-product. The severity rule is agnostic to which other
  problem category coexists with the mismatch; one representative second
  state is enough to pin it.

## Verification

- `just test-rust` -- the new test must pass on the current rule, and must
  fail if anyone weakens the rule to "Fail iff `uuid_mismatch` is the sole
  problem category" (manual confirmation: temporarily change line 492 to
  `if uuid_mismatch.is_empty() || (missing.is_empty() && not_block.is_empty()
  && probe_failed.is_empty() && header_unreadable.is_empty() &&
  header_damaged.is_empty()) == false` -- the new test should fail, the
  existing one should still pass; revert).
- No VM tests touched; `tests/cli/braid-doctor-uuid-swap.py` already covers
  the integration path against ADR-024 and is unaffected.
