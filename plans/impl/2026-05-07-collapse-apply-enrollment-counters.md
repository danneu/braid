# Plan: collapse `apply_enrollment` counters into post-loop counts

## Context

`cli/src/enroll_key_file.rs::apply_enrollment` maintains two `u32`
running totals (`enrolled`, `already`) inside a `for` loop over a
borrowed `&[DiskEnrollAction]` slice, only to print a single trailing
summary line:

```
done: {enrolled} enrolled, {already} had keyfile
```

The counters are pure bookkeeping -- they are not consulted inside the
loop, never affect control flow, and are only read by the final
`eprintln!`. Because `plan` is a borrowed slice it can be re-iterated
freely after the loop. Replacing the counters with two
`plan.iter().filter(...).count()` calls just before the `eprintln!`
removes the dead state and lets the `match` collapse to an `if let`,
since the `AlreadyEnrolled` arm becomes empty.

This is a readability/dead-state cleanup with no behavior change. The
proposed simplification was confirmed by `verify-issue` against the
current file.

## Files to change

- `cli/src/enroll_key_file.rs` -- only `apply_enrollment` (current
  body at lines ~240-290).

## The change

Replace the `apply_enrollment` body with:

1. Drop the `let mut enrolled = 0u32;` / `let mut already = 0u32;`
   declarations.
2. Convert the `match action { ... }` to
   `if let DiskEnrollAction::NeedsEnroll { name, by_id } = action`,
   since the `AlreadyEnrolled` arm has no remaining work once the
   counter increment is gone.
3. Compute the two counts immediately before the trailing
   `eprintln!`:

   ```rust
   let enrolled = plan
       .iter()
       .filter(|a| matches!(a, DiskEnrollAction::NeedsEnroll { .. }))
       .count();
   let already = plan
       .iter()
       .filter(|a| matches!(a, DiskEnrollAction::AlreadyEnrolled { .. }))
       .count();
   eprintln!(
       "done: {} enrolled, {} already had keyfile",
       enrolled, already
   );
   ```

   The format-string semantics are identical -- `usize` formats the
   same as the prior `u32` for these values.

Nothing outside `apply_enrollment` is touched. No public API, error
type, or trailing-message wording changes.

## Why this is safe

- `plan: &[DiskEnrollAction]` is borrowed, so re-iterating it after
  the apply loop is free.
- The `?` early-return paths inside the loop already skip the trailing
  `eprintln!` in the current code; they continue to do so unchanged,
  which means the post-loop counts are reached only on full success
  -- the same condition that produced the running totals.
- `cargo test` coverage of `apply_enrollment` lives in
  `cli/src/enroll_key_file.rs:2423-2619`
  (`apply_enrolls_needs_enroll_items`, `apply_skips_already_enrolled`,
  `apply_mixed_plan`,
  `apply_enrollment_returns_enriched_error_when_backup_fails`). None
  of them assert on the `done:` line or on the counter values, so
  behavior parity is preserved by construction.
- No other call site or test in the repo references the counters or
  the literal `"already had keyfile"` text (verified via grep).

## Verification

1. `just test-rust` -- runs the four `apply_enrollment` unit tests
   plus the rest of the CLI suite; should pass unchanged.
2. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
   from the workspace root -- the change is small enough that lint
   regressions are unlikely, but worth confirming.
3. Manual smoke (optional, only if VM testing is otherwise needed):
   `just test-vm` -- no enrollment-specific test asserts on the
   summary line, so this is a sanity check rather than a contract
   gate.

No fixture refresh, no doc updates, no decision-record changes.
