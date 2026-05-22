# Plan: rename `type_` -> `type` in `braid ups status --json`

## Context

`braid ups status --json` currently emits two JSON keys with a trailing
underscore -- `battery.type_` and `device.type_` -- because the
underlying Rust struct fields use the reserved-word escape `type_` and
no `#[serde(rename = ...)]` annotation maps them back. The escape is a
Rust naming concern that has no business in a script-facing JSON
contract, and the manual currently pins the awkward form
(`manual/commands/ups-status.md:48,65`), so users writing `jq` filters
have to spell the rust-escape rather than the natural NUT key
(`battery.type`, `device.type`).

braid is unreleased and `AGENTS.md` is explicit about "No backwards
compatibility", so the right time to fix this is before more downstream
scripts grow around the current shape. The lsblk parser already uses
`#[serde(rename = "type")]` for exactly the same reason
(`cli/src/parse/lsblk.rs:18`), so the fix matches an established
project pattern.

## Change

Add `#[serde(rename = "type")]` to both `type_` fields and update the
example JSON in the manual to mirror the new wire key.

### Critical files

- `cli/src/parse/types.rs` -- add `#[serde(rename = "type")]` above the
  `pub type_: Option<String>` field in `BatteryFields` (line 639) and
  in `DeviceFields` (line 667). Leave the Rust field name as `type_`
  so the existing internal call sites
  (`cli/src/parse/upsc.rs:83,95,287,300` and the `ups.rs` unit-test
  struct literals) continue to compile unchanged.
- `manual/commands/ups-status.md` -- in the `## JSON output` shape
  block, change `"type_": "PbAc"` (line 48) and `"type_": "ups"`
  (line 65) to `"type": "PbAc"` / `"type": "ups"`. `manual/book/` is
  gitignored mdbook output and regenerates from the source; no manual
  HTML edits needed.
- `cli/src/ups.rs` -- extend
  `json_online_fixture_has_expected_shape` (around line 798) so the
  rename is pinned by a behavioral test. Add three assertions:
  `assert_eq!(value["battery"]["type"], "PbAc")`,
  `assert_eq!(value["device"]["type"], "ups")`, and a guard that
  neither nested object still carries the old key, e.g.
  `assert!(value["battery"].get("type_").is_none())` and the same for
  `value["device"]`. The fixture
  (`cli/tests/fixtures/nixos-25.11/upsc/upsc-online.txt:6,11`) already
  publishes `battery.type: PbAc` and `device.type: ups`, so the
  assertions are accurate without touching the fixture. This is the
  one test that would otherwise silently keep passing if a future
  change removed `#[serde(rename = "type")]`.

### Out of scope

- No rename of the Rust field itself. `battery.type_` reads fine in
  the Rust code (the parent struct already disambiguates) and the
  `lsblk` precedent renames Rust only when the JSON key would
  otherwise be ambiguous at the top level, which is not the case
  inside the nested `battery` / `device` JSON objects.
- No internal call-site changes. All readers/writers use the
  `.type_` field accessor and are unaffected by `#[serde(rename)]`.
- No sibling refactor. A grep across `cli/src/` for other
  reserved-word fields (`type_`, `ref_`, `self_`, `impl_`, ...)
  turned up only these two instances, so there is no class of bug
  to dissolve.

## Verification

1. `just test-rust` -- confirms unit tests still pass and, with the
   new assertions in `json_online_fixture_has_expected_shape`,
   actively pins the rename. Without the `#[serde(rename = "type")]`
   annotations the new assertions on `value["battery"]["type"]` and
   `value["device"]["type"]` fail, so any future revert produces a
   visible test failure rather than silently regressing the wire
   format.
2. `just test-vm braid-status-ups` -- exercises the live CLI parser
   canary against NUT in a VM; neither `tests/cli/braid-status-ups.py`
   nor `tests/module/ups-preflight-on-battery.py` asserts on the
   `type` key today, so this should pass unchanged and proves nothing
   adjacent regressed.
3. Manual eyeball of the doc snippet: after editing, `grep -n '"type'
   manual/commands/ups-status.md` should show `"type":` (no trailing
   underscore) at the previously-noted lines, and no remaining
   `"type_"` occurrences anywhere under `manual/commands/`.
