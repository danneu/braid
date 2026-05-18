# Fix Current Rustfmt Drift

## Summary

Make the current Rust tree pass `cargo fmt --check` by applying only
rustfmt-equivalent formatting changes. The original doctor finding is
real, but the current tree also has rustfmt drift in
`cli/src/remove_missing.rs`, so the ideal fix must cover both files if
the goal is a clean full format check.

## Key Changes

- In `cli/src/doctor.rs`, split the long
  `assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Fail);`
  into rustfmt's multi-line form.
- In `cli/src/remove_missing.rs`, collapse the three
  `let steps = ... .render_steps();` test initializations around the
  rebalance render tests into rustfmt's preferred layout.
- Do not change assertions, helper arguments, test names, or behavior.
  This is formatting-only.

## Test Plan

- Run `cargo fmt --check` and require it to pass.
- No behavioral tests are required for the formatting-only fix.

## Assumptions

- The desired outcome is a clean full `cargo fmt --check` on the current
  working tree, not only fixing the originally cited `doctor.rs` line.
