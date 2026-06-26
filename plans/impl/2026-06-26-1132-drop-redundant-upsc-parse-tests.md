# Plan: drop the redundant in-module upsc fixture parse tests

**Status: Draft**

## Context

`cli/src/parse/upsc.rs::tests` carries two fixture-backed parse tests --
`parses_online_fixture` and `parses_lowbattery_fixture` -- that `include_str!`
the committed `upsc-online.txt` / `upsc-lowbattery.txt` fixtures and assert a
small set of core properties (OL present / OB+LB present, charge value, device
model). A review flagged these as redundant against the golden lane, and the
sweep below confirms it: every property and guarantee they provide is already
pinned by tests that are equally unconditional and, for the parse contract,
*authoritative*.

This is the natural next step of the already-implemented consolidation in
`plans/impl/2026-06-04-drop-redundant-ups-json-tests.md`. That plan deleted the
three redundant per-state `json_*_fixture_has_expected_shape` tests with no
replacement, leaning on the surviving snapshot + golden + all-tokens tests as
the contract. Once those snapshots and the golden lane are established as the
contract, the in-module *parse* fixture tests are redundant against them in
exactly the same way -- the redundancy argument simply moves up one layer.

Three places consume these fixtures today:

- **Parse contract (authoritative).** `cli/tests/support/golden_common.rs#golden_upsc_online`
  and `#golden_upsc_lowbattery`. `docs/dev/parser-compatibility.md` designates
  the committed `nixos-26.05` fixtures and the golden lane (`just test-rust`) as
  authoritative; no principle requires a separate in-module parse smoke test.
- **Render/serialization contract (unconditional, in-module).** `cli/src/ups.rs`
  `snapshot_human_online`, `snapshot_human_lowbattery`, and
  `json_online_fixture_has_expected_shape` -- plain `#[test]` fns with no skip
  guard that `include_str!` the same fixtures (so the compile-time
  fixture-existence pin survives) and parse them through `parse_upsc`.
- **In-module parse smoke (redundant -- this plan removes it).** The two tests
  above.

The intended outcome: less maintenance surface (a fixture-shape change stops
forcing edits in two lanes) with zero loss of behavioral coverage.

## Change

Delete two test functions, with their `// Intent / Why / Scenario` preambles,
from `cli/src/parse/upsc.rs::tests`:

- `cli/src/parse/upsc.rs#parses_online_fixture`
- `cli/src/parse/upsc.rs#parses_lowbattery_fixture`

Do **not** add a replacement smoke check. The existence pin + always-run parse
coverage that a smoke check would protect is already provided by the `ups.rs`
`include_str!` render/JSON tests (see table). This matches the 2026-06-04 plan's
clean-delete-with-no-replacement shape.

No other edit: the two tests are the last items in the module and have no shared
section-header comment, so nothing in `upsc.rs` goes stale. `parses_rich_model_fields`
and every inline parser unit test stay. No production code, fixtures, snapshots,
or docs change.

## Why coverage is preserved (per deleted assertion)

| Deleted assertion | Surviving coverage |
|---|---|
| online: `OL` present | `golden_upsc_online` (`contains(Ol)`); `json_online_fixture_has_expected_shape` (`OL` in `status_flags`); `snapshot_human_online` (`Status: OL`) |
| online: `OB` absent | `golden_upsc_online` (`status_flags.len() == 1` -- strictly stronger than `!contains(Ob)`); `snapshot_human_online` renders only `Status: OL` |
| online: `battery.charge_pct == 100` | `golden_upsc_online` (`== Some(100)`); `json_online_fixture_has_expected_shape` (`charge_pct == 100`); `snapshot_human_online` (`Battery: 100%`) |
| online: `device.model == "Back-UPS ES 550G"` | `golden_upsc_online`; `json_online_fixture_has_expected_shape`; `snapshot_human_online` (`Device: APC Back-UPS ES 550G`) |
| lowbattery: `OB` + `LB` present | `golden_upsc_lowbattery` (`contains(Ob)` + `contains(Lb)`); `snapshot_human_lowbattery` (`Status: OB LB`) |
| lowbattery: `battery.charge_pct == 8` | `snapshot_human_lowbattery` (byte-exact `Battery: 8%`); `golden_upsc_lowbattery` pins the deliberately robust `charge <= 10` |

Every deleted fact is pinned by at least one unconditional in-module test
(`snapshot_human_*` / `json_online_*`) **and** the authoritative golden lane.
The only assertion not retained verbatim by golden is the exact `charge_pct == 8`,
which golden intentionally relaxes to `<= 10` ("seeds charge below the low
threshold"). That exact value is retained by `snapshot_human_lowbattery`, which
is the correct home for a brittle fixture-specific value: it is byte-exact and
auto-maintained via `cargo insta review` on a fixture re-capture, rather than
hand-asserted in a second lane.

## Interaction with the 2026-06-04 plan (the one nuance)

`plans/impl/2026-06-04-drop-redundant-ups-json-tests.md` cites
`parses_lowbattery_fixture` as co-equal surviving coverage for two rows of its
table (lowbattery `OB`+`LB`; lowbattery `charge_pct == 8`) and in its
out-of-scope note ("`upsc-lowbattery.txt` ... via its human snapshot and
`parses_lowbattery_fixture`"). After this deletion, both facts remain pinned by
the `snapshot_human_lowbattery` source listed alongside the parse test in that
same table, so no coverage gap opens.

That plan is complete and its change is in the tree. Following its own stated
convention -- it refused to rewrite the older `2026-05-11` plan's inventory,
treating completed plans as point-in-time history -- this plan does **not** edit
the 2026-06-04 plan. This newer dated plan is the superseding record for the
parse-test portion of that plan's safety-net rationale.

## Explicitly out of scope (do not touch)

- **Fixtures.** Neither fixture is orphaned. `upsc-online.txt` stays referenced
  by `golden_upsc_online`, `snapshot_human_online`, and
  `json_online_fixture_has_expected_shape`; `upsc-lowbattery.txt` by
  `golden_upsc_lowbattery` and `snapshot_human_lowbattery` (both still
  `include_str!`/read it). No fixture file becomes unreferenced.
- **The golden lane and the `ups.rs` snapshot/JSON tests.** They are the
  contract this plan relies on; leave them unchanged. Do not "compensate" by
  tightening `golden_upsc_lowbattery` to `== 8` -- the robust `<= 10` is
  deliberate.
- **`parses_rich_model_fields` and the inline parser unit tests.** They remain
  the co-located, fixture-independent parser coverage; not touched.
- **Docs / ADRs.** No `docs/` page or ADR names these two tests; none requires
  an in-module per-fixture parse test. No doc edit required.
- **The completed `2026-06-04` and `2026-05-18` plans.** Left as history (see
  above).

## Verification

Pure Rust unit-test-lane change -- no VM run and no fixture capture (no parser,
production, nixpkgs, or `cli/tests/fixtures/**` change).

```sh
# the parser module's surviving unit tests pass (two fewer than before)
cargo test --manifest-path cli/Cargo.toml --lib parse::upsc::tests
# the safety-net render/JSON tests are still green and snapshots unchanged
cargo test --manifest-path cli/Cargo.toml --lib ups::tests
# no unused imports / dead refs left behind (use super::* stays used by other tests)
cargo check --manifest-path cli/Cargo.toml --tests
# full Rust unit gate, including the authoritative golden lane for parse_upsc
just test-rust
```

Expected: `parse::upsc::tests` runs with `parses_online_fixture` and
`parses_lowbattery_fixture` gone, all other parser tests green; `ups::tests`
unchanged and green with snapshots clean; `cargo check --tests` warning-free
(the deleted tests shared no unique import); `just test-rust` green, with the
golden lane continuing to assert the online/lowbattery parse contract.

## Critical files

- `cli/src/parse/upsc.rs` -- delete `parses_online_fixture` and
  `parses_lowbattery_fixture` (with their preambles). Everything else in the
  module unchanged.
