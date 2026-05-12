# Tighten `btrfs replace status` Parser

## Summary

Rewrite `parse_btrfs_replace_status` as a strict, anchored `nom` parser while
preserving the current `ReplaceState` API and fail-closed behavior. The pivot is
to fix the real remaining issue: substring parsing and incomplete real-fixture
coverage, without reintroducing stale leniency for empty output or
`no operation running`.

## Key Changes

- Replace the current `contains` / `find` / `rfind` parser with a private
  `parse_replace_status_line(input: &str) -> IResult<&str, ReplaceState>`.
- Parse exactly one trimmed stdout line; reject empty output, embedded line
  breaks, trailing junk, and unrecognized text with `ParseError::InvalidText`.
- Keep the existing enum variants and caller behavior: `Never started`,
  running, finished, canceled, and suspended map to the current `ReplaceState`
  variants.
- Parse the upstream error-counter suffix for all states that include it:
  `, <n> write errs, <n> uncorr. read errs`. Do not expose those counters in
  public types.
- Remove `extract_percent`; parse progress with a dedicated percent parser that
  accepts only the upstream `progress2string` shape: ASCII digits, `.`, exactly
  one ASCII digit, and `%`. Reject signs, exponent forms, `NaN`, `inf`,
  non-finite values, and values outside `0.0..=100.0`.
- Update the `ReplaceState::NotStarted` doc comment so it no longer claims
  empty or idle output is accepted.

## Fixture Coverage

- In `tests/capture-tool-fixtures.py`, when the canceled-fixture setup first
  observes running replace status, write that exact `last_status` to
  `btrfs-replace-status-running.txt` before issuing `btrfs replace cancel`.
- Add a shared golden test for `btrfs-replace-status-running.txt` in
  `golden_common.rs`; this covers both stable and unstable lanes once fixtures
  are refreshed.
- Do not add a suspended real fixture in this pivot. Keep suspended as inline
  synthetic coverage because the repo does not currently have a deterministic
  suspended-state fixture capture path.

## Test Plan

- Update parser unit tests so valid inline cases still cover all five states.
- Add or adjust negative unit tests for empty stdout, `no operation running`,
  invalid percent forms, missing suspended percentage, extra trailing text, and
  random text; all should return `InvalidText`.
- Add one table-driven strictness test for partial, multiline, and
  counterless outputs, including `prefix Never started`, `Never started\njunk`,
  and `45.3% done`.
- Refresh fixtures with `just capture-all-fixtures` and
  `just capture-all-fixtures-unstable`.
- Run `just test-rust` and `just test-rust-unstable`.

## Assumptions

- No public API/type changes are needed.
- `btrfs-progs` source remains the contract authority: successful
  `replace status -1` emits one of the documented state lines, not empty
  output.
- The best scope is parser strictness plus the low-cost running fixture;
  deterministic suspended fixture capture is separate work if needed later.
