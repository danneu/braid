# Plan: document the lossless-cast invariant behind `watts_estimated`

## Context

A review finding (Low / Testing) flagged that the test
`watts_estimated_handles_max_nominal_watts` asserts `Some(u32::MAX)` with no
explanation, so a reader cannot tell whether that value is the intended
contract or an accident of the arithmetic.

Investigation confirmed the finding's framing is slightly off but its core ask
is sound:

- `UpscOutput::watts_estimated` (`cli/src/parse/types.rs`) computes
  `((u64::from(pct) * u64::from(nominal) + 50) / 100) as u32`. The `as u32` is
  **lossless, not saturating**: `parse_pct` (`cli/src/parse/upsc.rs`) gates
  `load_pct` to `0..=100`, so the rounded quotient is at most `nominal` (equal
  at 100% load) and always fits `u32`. The asserted `u32::MAX` is the
  "100% load => est == nominal" identity, not a clamp.
- This reasoning **already exists** in the implemented design doc
  `plans/impl/2026-05-26-overflow-safe-display-diagnostics.md`:
  *"`pct <= 100` so the result is at most `u32::MAX`
  (`(100 * u32::MAX + 50) / 100 == u32::MAX`), making the cast lossless."*
  It simply was never surfaced into the code, which is why the value reads as a
  magic constant.

The intended outcome: move that decided rationale from the plan doc into the two
places a reader actually looks -- the method's `///` and the test -- so the
asserted boundary is self-evident. This is the `docs` shape of the fix; AGENTS.md
requires every `pub` item's `///` to state its load-bearing invariant, and this
cast's safety invariant is currently undocumented at the call site.

### Considered and rejected: make the cast saturating

Changing `as u32` to `u32::try_from(..).unwrap_or(u32::MAX)` was considered and
**rejected**. The design doc deliberately partitions overflow-safety by return
shape: `Option`/`Result` parsers fail closed, diagnostic counters saturate, and
**bare-integer returns that provably fit widen + cast back**. `watts_estimated`
is the sanctioned instance of the third bucket. A saturating cast would only
change behavior for `load_pct > 100`, which `parse_pct` cannot produce, while
contradicting a decided ADR. No clippy cast lint is enabled, so there is no lint
pressure either. The bug the test actually guards -- re-narrowing the arithmetic
back to `u32`, which panics in debug at this input -- is unaffected.

## Changes

Two comment-only edits. No behavior change, no new test, no signature change.

### 1. `cli/src/parse/types.rs` -- `watts_estimated`

Extend the existing `///` with the invariant that makes the cast safe. Append a
paragraph after the current doc, in substance:

> The u64 widening and `as u32` cast are lossless, not truncating: `parse_pct`
> gates `load_pct` to `0..=100`, so the rounded quotient
> `(pct * nominal + 50) / 100` is at most `nominal` (equal at 100% load) and
> always fits `u32`. `+ 50` before `/ 100` rounds to nearest.

This states the cross-file invariant (`parse_pct` gate) that a reader of
`types.rs` otherwise cannot see, and names the rounding so `+ 50` is not a magic
number.

### 2. `cli/src/parse/upsc.rs` -- `watts_estimated_handles_max_nominal_watts`

Add an inline arithmetic comment at the assertion, matching the established style
of the sibling test `watts_estimated_requires_both_ingredients`
(`// 50 * 330 = 16500, / 100 = 165 ...`):

```rust
    // At 100% load, est == nominal: (100 * 4294967295 + 50) / 100 = 4294967295
    // = u32::MAX. This is the lossless boundary, not a saturating clamp.
    assert_eq!(out.watts_estimated(), Some(u32::MAX));
```

Optionally tighten the test's `// Why it exists:` line to name the regression it
guards explicitly -- re-narrowing the widened arithmetic back to `u32`, which
overflow-panics in debug at `100 * u32::MAX`. The current wording ("debug
overflow checks must not panic") is accurate but does not name the mechanism.

Keep ASCII only (`--`, `'`, `...`, `<=`), per the global writing-style rule.

## Verification

- `just test-rust` -- the suite must stay green. Behavior is unchanged, so any
  failure would mean a typo in a doc comment (e.g. a broken intra-doc link) or an
  accidental code edit. Confirm `watts_estimated_handles_max_nominal_watts` and
  `watts_estimated_requires_both_ingredients` still pass.
- `cargo doc -p braid-cli --no-deps` (the CLI crate is `braid-cli` per
  `cli/Cargo.toml#package`, not `braid`) renders cleanly, confirming the expanded
  `///` compiles.
- Spot-check no Unicode crept in: the edited comments use `--`/`<=`/ASCII only.

## Critical files

- `cli/src/parse/types.rs` -- `watts_estimated` `///` (edit).
- `cli/src/parse/upsc.rs` -- `watts_estimated_handles_max_nominal_watts` comment
  (edit); sibling `watts_estimated_requires_both_ingredients` is the style
  reference (no edit).
- `plans/impl/2026-05-26-overflow-safe-display-diagnostics.md` -- source of the
  invariant being surfaced (no edit).
