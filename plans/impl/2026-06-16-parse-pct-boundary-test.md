# Pin parse_pct's fractional / 100.x boundary policy with a test

## Context

`parse_pct` in `cli/src/parse/upsc.rs#parse_pct` converts a NUT percent string
(`battery.charge`, `ups.load`) into `Option<u8>` using a deliberate, documented
policy: split on `.`, parse **only** the integer part, discard the fraction, and
gate on `intpart > 100`. The docstring (`cli/src/parse/upsc.rs#parse_pct`) and a
dedicated doc-only commit (`bcbc435b docs(ups): clarify parse_pct boundary
policy for 100.x`) spell out the two non-obvious arms:

- `99.9 -> Some(99)` -- floored, **not** rounded (conservative on not-quite-full).
- `100.5 -> Some(100)` -- borderline firmware overshoot tolerated as 100.
- `101.x -> None` -- the gate fires on the integer part.

The behavior is correct and intentional, so **no code change is warranted**. The
gap is purely in tests: the only numeric-edge test, `pct_out_of_range_is_none`
(`cli/src/parse/upsc.rs#pct_out_of_range_is_none`), covers far-out integers `200`/`999`. Every
other charge/load value in the suite and in all fixtures is a clean integer
(`8, 17, 100, ...`). So the floor-after-split and the `100.x` tolerance band --
the exact arms the docstring calls out -- have zero coverage.

This makes the contract silently refactor-fragile: a swap to
`s.parse::<f64>()` + `round()` (turning `99.9 -> 100`) or to `s.parse::<u8>()`
(turning `99.9`/`100.5-> None`, since `u8` rejects a decimal string) would pass
every existing test while breaking the documented promise. Both `charge_pct` and
`load_pct` feed the human `Battery`/`Load` lines (`cli/src/ups.rs#format_human`) and
the TUI (`cli/src/tui/browse/view.rs#ups_status_lines`), so a near-full misrender would
slip through. Intended outcome: a behavioral, structure-insensitive test that
pins these arms so the regression cannot land unnoticed.

## Change

Add **one** new `#[test]` to the `#[cfg(test)] mod tests` block in
`cli/src/parse/upsc.rs`, placed next to the existing `pct_out_of_range_is_none`
so the two boundary tests sit together. No production code changes.

Design decisions (all matching house style in this file):

- **Drive through `parse_upsc`, not the private `parse_pct`.** This keeps the
  test structure-insensitive (it asserts on the public parser's output, the same
  contract `braid ups status` and the TUI consume) and exercises **both** call
  sites at once: `battery.charge -> battery.charge_pct` and
  `ups.load -> load_pct`.
- **Bundle the related boundary assertions in one test.** `pct_out_of_range_is_none`
  already bundles `200` and `999`; mirroring that keeps one focused preamble per
  behavior rather than fragmenting "the parse_pct boundary policy" across tests.
- **Cases pinned** (each maps to a plausible refactor it would catch):
  - `99.9 -> Some(99)` -- floor, not round (kills the `f64 + round()` refactor).
  - `100.5 -> Some(100)` -- tolerance band (kills the `u8::parse` refactor).
  - `101.0 -> None` -- gate fires on the integer part even with a fractional tail.
  - `101 -> None` (bare integer) -- the exact min-rejected boundary, tightening
    the existing test which only proves the far-out `200`/`999`.
- **Mandatory preamble.** Per `docs/dev/testing.md`, every test opens with an
  `Intent` / `Why it exists` / `Scenario` `//` comment block; the new test must
  carry one (the finding omitted this). Reference commit `bcbc435b` and the
  docstring in the `Why it exists` line.
- **Name:** snake_case, behavior-describing, e.g.
  `pct_floors_fraction_and_gates_on_intpart` (cf. `pct_out_of_range_is_none`).

### Sketch (final wording to be refined at implementation)

```rust
// Intent: parse_pct splits on '.', floors the fraction, and gates on the
// integer part alone -- 99.9 -> Some(99), 100.5 -> Some(100), 101.x -> None.
// Why it exists: the floor-after-split and 100.x-tolerance arms are a
// deliberate, documented policy (parse_pct docstring; commit bcbc435b), but
// every charge/load fixture uses clean integers. A refactor to f64::parse +
// round() (99.9 -> 100) or to u8::parse (99.9/100.5 -> None) would pass every
// other test while silently breaking this contract; both battery.charge_pct
// and load_pct feed the human Battery/Load lines and the TUI, so a near-full
// misrender would slip through.
// Scenario: a driver reports fractional charge/load just under and just over
// 100% (firmware FP drift), alongside a genuinely out-of-range 101.
#[test]
fn pct_floors_fraction_and_gates_on_intpart() {
    // Fractional part is discarded, not rounded: 99.9 floors to 99.
    let near_full = parse_upsc("battery.charge: 99.9\nups.load: 100.5\n");
    assert_eq!(near_full.battery.charge_pct, Some(99));
    // 100.x is tolerated as 100 (intpart <= 100), not rejected.
    assert_eq!(near_full.load_pct, Some(100));
    // The gate operates on the integer part, so 101.x trips it even with a
    // fractional tail; the bare 101 pins the exact min-rejected boundary that
    // pct_out_of_range_is_none (200/999) leaves far from the edge.
    let over = parse_upsc("battery.charge: 101.0\nups.load: 101\n");
    assert_eq!(over.battery.charge_pct, None);
    assert_eq!(over.load_pct, None);
}
```

## Files to modify

- `cli/src/parse/upsc.rs` -- add the one test inside `mod tests`, adjacent to
  `pct_out_of_range_is_none` (`cli/src/parse/upsc.rs#pct_out_of_range_is_none`).
  No other files.

## Verification

1. Confirm the test fails against a broken parser before trusting it (TDD
   sanity check): temporarily imagine/patch `parse_pct` to `s.parse::<f64>()`
   then `round()` and to `s.parse::<u8>()` -- each should fail at least one of
   the four assertions. (Do this only as a scratch check; revert.)
2. Run the focused test:
   `cargo test --lib pct_floors_fraction_and_gates_on_intpart`
3. Run the module and the full Rust suite to confirm no regressions:
   `cargo test --lib parse::upsc` then `just test-rust`.
4. `cargo fmt` (no `just` fmt lane exists; `fmt-nix` covers only `.nix`),
   then `just clippy` to keep the tree clean. Use `just clippy`, not a bare
   `cargo clippy`: the recipe runs `cargo clippy --manifest-path cli/Cargo.toml
   --tests` (`justfile#clippy`), and the `--tests` flag is required to lint the
   new `#[cfg(test)]` code -- a default `cargo clippy` can skip the test target
   entirely. Commit `37ed8136` shows fmt is enforced.

No fixture refresh or NixOS VM run is needed -- this is a pure-Rust unit test
using inline string input, not a parser-compatibility or VM-canary change.
