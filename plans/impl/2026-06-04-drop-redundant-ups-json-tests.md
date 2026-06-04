# Plan: drop the redundant per-state `json_*_fixture_has_expected_shape` tests

**Status: Draft**

## Context

`cli/src/ups.rs::tests` carries four `json_<state>_fixture_has_expected_shape`
tests (online, onbattery, lowbattery, replace-battery). They are the *original*
coarse JSON checks from the feature commit (`dfe7af9b feat(ups): add TUI panel,
rich braid ups status, and doctor checks`). Since then the UPS JSON suite was
curated through a series of small, precise tests that now subsume the per-state
ones:

- `cli/src/ups.rs#json_output_status_flags_preserve_insertion_order` serializes
  **all 17 flag variants** (OL, OB, LB, RB, ...) through `JsonReport::success`
  and pins each NUT token verbatim and in order -- strictly stronger, on the
  exact JSON path, than any per-state "OB is present" membership check.
- `cli/src/ups.rs#snapshot_json_success_keeps_sparse_typed_keys` pins the
  null-not-omitted contract and the `type`/`type_` rename (snapshot shows
  `"type": null`).
- `cli/src/ups.rs#json_online_fixture_has_expected_shape` pins the full
  populated key set through serde from a real fixture, plus the `type`/`type_`
  rename with real values and the explicit `type_`-absent assertion.

The result is that onbattery/lowbattery/replace-battery JSON tests re-prove a
strict subset of surviving coverage, while each remains a separate hand-written
assertion block that must be re-checked on every captured-fixture refresh
(nixpkgs bump touching NUT). They are also the only three tests in the module
that lack the mandatory three-section `// Intent / Why / Scenario` preamble --
a corroborating sign they were tacked on.

Outcome: delete the three redundant per-state JSON tests; keep the online one as
the single per-fixture JSON-shape anchor. Net result is less maintenance surface
with zero loss of behavioral coverage.

## Change

Delete these three test functions from `cli/src/ups.rs::tests` (each is a bare
`#[test] fn` with no preamble comment of its own):

- `json_onbattery_fixture_has_expected_shape`
- `json_lowbattery_fixture_has_expected_shape`
- `json_replace_battery_fixture_has_expected_shape`

Keep `json_online_fixture_has_expected_shape` unchanged. Leave the adjacent
`snapshot_human_onbattery` / `snapshot_human_lowbattery` /
`snapshot_human_replace_battery` tests and their `// Intent / Why / Scenario`
preambles intact -- the preamble blocks belong to the human-snapshot tests, not
to the JSON tests being removed.

Then update the now-stale block comment above the fixture-backed tests (the
`// --- Fixture-backed render snapshots ---` header in `cli/src/ups.rs::tests`).
Its second paragraph currently claims:

> Each snapshot test also JSON-serializes the parsed model so the `--json`
> contract is covered from the same fixture. This double coverage is cheap (one
> parse, two serializers) and guards the two outputs against drift relative to
> each other.

That stops being true once only `json_online_fixture_has_expected_shape`
remains -- the onbattery/lowbattery/replace fixtures keep only their human
snapshot. Replace that paragraph so it says the **online** fixture is the one
that also JSON-serializes its parsed model (anchoring the `--json` contract
against a real fixture), while per-state JSON shape is not re-checked here:
status-token serialization is covered by
`json_output_status_flags_preserve_insertion_order` and sparse field presence by
`snapshot_json_success_keeps_sparse_typed_keys`. Final wording is the
implementer's; the requirement is that it no longer claims *every* snapshot test
serializes JSON and that it points at the surviving online + shared-unit
coverage.

The only edits are inside `cli/src/ups.rs::tests`: delete three test functions
and rewrite that one block comment. No production code, fixtures, snapshots, or
docs change.

## Why coverage is preserved (per deleted assertion)

| Deleted assertion | Surviving coverage |
|---|---|
| onbattery: `OB` present, `LB` absent | `snapshot_human_onbattery` (byte-exact `Status: OB`); `json_output_status_flags_preserve_insertion_order` pins OB's JSON token |
| onbattery: `input.voltage == "0.0"` | `snapshot_human_onbattery` (`Input: 0.0 V`) pins the parsed value; `json_online_fixture_has_expected_shape` pins serde's `input.voltage` handling (String passthrough -- "0.0" cannot serialize differently from "120.0") |
| lowbattery: `OB` + `LB` present | `snapshot_human_lowbattery` (`Status: OB LB`); `cli/src/parse/upsc.rs#parses_lowbattery_fixture`; the all-tokens JSON test |
| lowbattery: `battery.charge_pct == 8` | `parses_lowbattery_fixture` (`charge_pct == Some(8)`) + `snapshot_human_lowbattery` (`Battery: 8%`); charge_pct JSON number serialization pinned by online (100) and `json_output_with_empty_status_has_warning_and_body` (55) |
| replace: `OL` + `RB` present, `OB` absent | `snapshot_human_replace_battery` (byte-exact `Status: OL RB`); the all-tokens JSON test pins OL/RB tokens |

Every deleted assertion's underlying fact is pinned by a surviving snapshot,
parser-golden, or all-tokens JSON test. Nothing unique to the JSON serialization
path is lost: the per-state tests only varied *values* of fields whose serde
handling is already exercised by the online and all-tokens tests.

## Explicitly out of scope (do not touch)

- **Fixtures.** All three `.txt` fixtures stay referenced after deletion
  (`upsc-onbattery.txt` and `upsc-replace-battery.txt` via their
  `snapshot_human_*` tests; `upsc-lowbattery.txt` via its human snapshot and
  `parses_lowbattery_fixture`). None is orphaned.
- **Docs.** No `docs/` page names these tests; the `--json` contract lives
  behaviorally in `docs/design/decisions/020-ups-integration.md` and
  `docs/commands/ups-status.md`. No doc edit required.
- **`plans/impl/2026-05-11-ups-test-fixtures.md`.** This is a *completed*
  scaffolding-migration plan (executed in commit `e1104192`). Its "eight
  snapshot pairs" / "four `json_*`" enumeration is a point-in-time record of
  what that migration saw, and its "leave all eight unchanged" decision was
  scoped to *not hiding fixture paths behind a helper* -- not to whether all
  four JSON tests must exist forever. Leave it untouched as history; do not
  rewrite its inventory to say "five."
- **The online test's individual `// Intent / Why / Scenario` preamble.**
  Distinct from the section block comment above it, which *is* rewritten (see
  Change). The online test's own preamble already explains why it uses
  structural assertions and what it pins, and it stays accurate because that
  test is unchanged -- do not bolt a pointer to the all-tokens test onto it.

## Verification

Pure Rust-unit-test-lane change -- no VM run and no fixture capture (no parser,
production, nixpkgs, or `cli/tests/fixtures/**` change).

```sh
# surviving ups unit tests still pass
cargo test --manifest-path cli/Cargo.toml --lib ups::tests
# no unused imports / dead refs left behind by the deletion
cargo check --manifest-path cli/Cargo.toml --tests
# full Rust unit gate
just test-rust
```

Expected: `parse_fixture`, `JsonReport::success`, and `serde_json` remain used by
surviving tests (`json_online_fixture_has_expected_shape`,
`json_output_status_flags_preserve_insertion_order`, the `snapshot_human_*`
tests), so `cargo check --tests` stays warning-free. `ups::tests` runs with three
fewer tests, all others green and snapshots unchanged.

## Critical files

- `cli/src/ups.rs` -- delete the three `json_<state>_fixture_has_expected_shape`
  test functions, and rewrite the `// --- Fixture-backed render snapshots ---`
  block comment to drop the "each snapshot test also JSON-serializes" claim.
  Everything else in the module unchanged.
