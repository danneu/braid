# Pin the resolve-before-relocation ordering on identity-refusal tests

## Context

`plan_remove_missing` (`cli/src/remove_missing.rs`) deliberately resolves the
removal target's identity *before* probing for relocation space:

- `load_membership` (`:438`) -- gate 7, rejects corrupt/duplicate-devid pool.json
- `resolve_removal_target` (`:447`) -- gate 8, rejects never-enriched/foreign devid
- `check_relocation_space` (`:468`) -- gate 9, the **only** caller of
  `CmdRequest::BtrfsDeviceUsageRaw` (`btrfs device usage --raw`, spawned at `:555`)

This ordering is intentional (committed in `7c154922 fix(remove-missing): resolve
missing devid before dry-run`) and matters behaviorally: on a real degraded NAS that
is *also* short on relocation space, resolving identity first guarantees the operator
sees the actionable identity error (corrupt pool.json / devid not in pool) rather than
a red-herring "ENOSPC pre-flight" relocation error. It also avoids spawning a btrfs
subprocess against an already-doomed command.

The gate-5 "wrong-id" test (`plan_remove_missing_rejects_wrong_missing_id_from_pool_state`,
`:1768`) pins this invariant for its case with an explicit assertion (`:1833`):

```rust
assert!(
    !log.lock().unwrap().iter()
        .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
    "wrong-id validation must not call BtrfsDeviceUsageRaw"
);
```

The sibling gate-7 and gate-8 refusal tests do **not**. They assert that *mutating*
requests are absent (`BtrfsDeviceRemove`, `BtrfsBalanceRaid1Soft`, `CryptsetupClose`,
`BtrfsDeviceScanForget`) but let the read-only `BtrfsDeviceUsageRaw` probe through. A
regression that reordered `check_relocation_space` ahead of identity resolution would
still pass these tests: the shared mock (`usage_raw_three_disk_one_missing()`,
`cli/src/test_fixtures/remove_missing.rs:57`) reports devid 3 as missing with adequate
survivor space, so the relocation probe would *succeed*, then identity resolution would
fail with the same pinned error -- the only observable difference being the extra probe
the tests don't check for.

This change pins the resolve-before-relocation ordering uniformly across the
identity/membership-resolution refusal family, matching the wrong-id precedent.

## Scope

Exactly **three** tests, all in `cli/src/remove_missing.rs` (complete set confirmed by
file-wide inventory):

1. `cmd_remove_missing_never_enriched_refusal_returns_structured_error` (`:3365`, gate 8)
2. `cmd_remove_missing_never_enriched_refusal_in_dry_run` (`:3448`, gate 8) -- the
   dry-run sibling; dry-run runs the full plan (relocation probe included) before the
   `if params.dry_run` branch in `cmd_remove_missing` (`:520`), so it shares the gap.
3. `cmd_remove_missing_duplicate_devid_pool_json_refused_at_load` (`:3533`, gate 7)

**Out of scope (deliberate):**

- The two direct `resolve_removal_target()` unit tests (`:2547`, `:2579`) call the
  function in isolation with no `CommandRunner`, so the relocation probe is structurally
  unreachable from them -- nothing to assert.
- Gate-4 (`missing_count == 0`) and gate-6 (2-device RAID1 guard) refusal tests
  (`:3015`, `:1012`, `:1077`). These guard *different* invariants and refuse far earlier;
  extending the assertion there is unrelated scope creep, not part of the
  identity-resolution root cause.

## Change

Add a standalone "must not call `BtrfsDeviceUsageRaw`" assertion to each of the three
tests, placed immediately after the existing no-mutation assertion.

**Keep it a separate assertion -- do NOT fold `BtrfsDeviceUsageRaw` into the existing
`... must issue zero mutating requests ...` block.** `BtrfsDeviceUsageRaw` is a
read-only probe, not a mutation; the existing block's message ("zero mutating requests")
would become inaccurate. The two invariants -- "no mutations" and "no relocation probe
before identity resolution" -- are distinct and each deserves its own assertion with an
accurate message, exactly as the wrong-id test keeps it standalone.

### Tests 1 and 2 (never-enriched + dry-run sibling)

Both already bind `let calls = runner.requests();` for the no-mutation assertion. Reuse
`calls` and append:

```rust
assert!(
    !calls
        .iter()
        .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
    "never-enriched refusal must precede the relocation-space probe \
     (resolve_removal_target before check_relocation_space); calls: {calls:?}"
);
```

For the dry-run sibling (test 2), prefix the message with `dry-run ` to match that
test's existing message convention.

### Test 3 (duplicate-devid)

This test has no `calls` binding (it inlines `runner.requests()`). Bind it once or
inline a parallel assertion after the existing `must not call btrfs device remove`
check:

```rust
assert!(
    !runner
        .requests()
        .iter()
        .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
    "duplicate-devid refusal must precede the relocation-space probe \
     (load_membership before check_relocation_space)"
);
```

Message wording should match the surrounding idiom; the literal text above is a
suggestion, not a constraint. Test code is exempt from the `check-output-ascii.py`
gate, but keep messages plain ASCII per house style anyway.

## Files

- `cli/src/remove_missing.rs` -- the only file modified (three test functions).

## Verification

1. **Tests pass as-is** (assertions hold today because all three genuinely refuse
   before gate 9):
   ```
   cargo test -p braid-cli cmd_remove_missing_never_enriched
   cargo test -p braid-cli cmd_remove_missing_duplicate_devid_pool_json_refused_at_load
   ```
   or the full Rust lane: `just test-rust`.
   (Confirm the crate/package name from the workspace if `-p braid-cli` is wrong; the
   bare `cargo test <name>` filter also works.)

2. **Confirm the assertion is non-vacuous (it actually guards the invariant).** As a
   temporary local experiment only -- do not commit -- reorder `plan_remove_missing`
   so the `check_relocation_space` block (`:468`) runs *before* `load_membership`
   (`:438`)/`resolve_removal_target` (`:447`), then re-run the three tests and confirm
   each now FAILS on the new `BtrfsDeviceUsageRaw` assertion (the error-message
   assertions still pass, proving the old tests would not have caught the reorder).
   Revert the reorder. This is the AGENTS.md "confirm it fails for the right reason"
   TDD step adapted to a guard-strengthening change.

No fixture refresh, doc, or README updates are required -- this is test-only hardening
with no behavior or parser-contract change.
