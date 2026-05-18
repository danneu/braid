# Refactor: collapse the legacy `tests/fixtures/nut/` lane

## Context

Two parallel `upsc` fixture trees co-exist in the repo:

- `cli/tests/fixtures/nut/` -- three hand-edited legacy files
  (`upsc-online.txt`, `upsc-onbattery-low.txt`, `upsc-daemon-down.stderr`).
- `cli/tests/fixtures/nixos-25.11/upsc/` -- five captured files
  (`upsc-online.txt`, `upsc-onbattery.txt`, `upsc-lowbattery.txt`,
  `upsc-replace-battery.txt`, `upsc-daemon-down.stderr`).

Only the captured lane is refreshed by `just capture-ups-fixtures`
(`tests/capture-ups-fixtures.py` writes to `nixos-25.11/upsc/`). The
legacy lane is hand-maintained, has no refresh path, and pins parser
smoke tests against ground truth that drifts independently of the live
tool. If NUT's `key: value` output ever changes (new fields, casing
tweaks, etc.), the captured-lane tests catch the drift; the legacy-lane
smoke tests keep passing on stale input.

Two sources of truth for the same parse contract is the only reason
this directory exists. Eliminating it removes that drift surface and
gives the parser smoke tests the same ground truth as the golden tests
in `cli/tests/support/golden_common.rs`.

Intended outcome: one fixture lane (`cli/tests/fixtures/nixos-25.11/upsc/`),
two updated parser smoke tests pointing at it, no behavior change.

## Scope (what changes)

1. **Repoint the two parser smoke tests** at `cli/src/parse/upsc.rs:328` and
   `:342` from `../../tests/fixtures/nut/...` to
   `../../tests/fixtures/nixos-25.11/upsc/...`.
2. **Rename one test** from `parses_onbattery_low_fixture` to
   `parses_lowbattery_fixture` so the test identifier matches the captured
   fixture basename (`upsc-lowbattery.txt`). The other test
   (`parses_online_fixture`) keeps its name -- the captured fixture's
   basename (`upsc-online.txt`) is identical to the legacy one.
3. **Update preamble text** that references the legacy filename or its
   "hand-written" provenance. See file delta below for the exact spans.
4. **Delete `cli/tests/fixtures/nut/` entirely** -- all three files:
   `upsc-online.txt`, `upsc-onbattery-low.txt`, `upsc-daemon-down.stderr`.
   The `.stderr` file is unreferenced by any source consumer
   (`cli/src/test_fixtures/ups.rs:46-56` hardcodes the connection-refused
   stderr inline; `cli/tests/support/golden_common.rs:578` already reads
   the captured copy via `upsc_fixture("upsc-daemon-down.stderr")`).

## Out of scope

- The captured fixture basename `daemon-down` stays as-is. The impl plan
  at `plans/impl/2026-05-04-split-upsc-query-parsing.md:463-472` explains
  why renaming the basename is undesirable (history churn, requires
  recapturing the lane). That rationale only protects the basename, not
  the legacy directory.
- The hardcoded inline stderr in `cli/src/test_fixtures/ups.rs:33-56` is
  not moved into a fixture file. Those helpers (with and without trailing
  newline) exist to test stderr trimming behavior at the byte level; an
  on-disk fixture cannot encode both newline variants cleanly. Leave them
  inline.
- The compile-time `include_str!` lock to `nixos-25.11` matches existing
  project convention -- the runtime `fixture()` helper in
  `cli/tests/support/golden_common.rs:466` already handles cross-lane
  resolution for the golden tests via the `REQUIRE_FIXTURES` const wired
  through `cli/tests/golden_nixos_25_11.rs` and
  `cli/tests/golden_nixos_unstable.rs`. No change to that mechanism.

## File deltas

### `cli/src/parse/upsc.rs`

**Test 1** (`parses_online_fixture`, currently lines 321-334):

- Line 321 (Intent): rewrite "minimal hand-written fixture" --> "captured
  fixture" so the provenance description stays accurate. The
  on-utility-power state assertion is unchanged.
- Line 325 (Scenario): keep the reference to `upsc-online.txt` -- the
  captured fixture uses the same basename.
- Line 328 (`include_str!`): change path to
  `"../../tests/fixtures/nixos-25.11/upsc/upsc-online.txt"`.

The four assertions on lines 330-333 (Ol present, Ob absent,
`charge_pct == Some(100)`, `device.model == "Back-UPS ES 550G"`) are all
satisfied by the captured `upsc-online.txt`.

**Test 2** (`parses_onbattery_low_fixture` -> `parses_lowbattery_fixture`,
currently lines 336-347):

- Line 336 (Intent): change `upsc-onbattery-low.txt` -->
  `upsc-lowbattery.txt`.
- Line 341 (`fn parses_onbattery_low_fixture()`): rename to
  `fn parses_lowbattery_fixture()`.
- Line 342 (`include_str!`): change path to
  `"../../tests/fixtures/nixos-25.11/upsc/upsc-lowbattery.txt"`.

The three assertions on lines 344-346 (Ob present, Lb present,
`charge_pct == Some(8)`) are all satisfied by the captured
`upsc-lowbattery.txt`.

### Files to delete

```
cli/tests/fixtures/nut/upsc-online.txt
cli/tests/fixtures/nut/upsc-onbattery-low.txt
cli/tests/fixtures/nut/upsc-daemon-down.stderr
```

After the file deletions, `cli/tests/fixtures/nut/` will be empty; remove
the directory.

## Reused references (no new abstractions)

- Captured fixtures already exist at
  `cli/tests/fixtures/nixos-25.11/upsc/upsc-online.txt` and
  `cli/tests/fixtures/nixos-25.11/upsc/upsc-lowbattery.txt`. No new files
  written.
- The `include_str!` pattern at parser-test scope already matches sibling
  parser tests in `cli/src/parse/` (e.g. `parse_lsblk`,
  `parse_btrfs_balance_status`, `parse_btrfs_scrub_status_per_device`,
  `parse_smartctl`). No new helper introduced.
- The renamed test name `parses_lowbattery_fixture` follows the
  `parses_<adjective>_fixture` pattern already used in
  `cli/src/parse/btrfs_scrub_status_per_device.rs::parses_running_fixture`.

## Verification

1. `just test-rust` -- run the full Rust unit-test suite. Both edited
   tests (`parses_online_fixture`, `parses_lowbattery_fixture`) must pass
   against the repointed fixtures. The four golden tests in
   `cli/tests/support/golden_common.rs:482-567` and the snapshot tests in
   `cli/src/ups.rs:792+` are untouched and must continue to pass.
2. `grep -rn 'fixtures/nut' cli/ tests/ scripts/ docs/ justfile flake.nix
   modules/ 2>/dev/null` -- must return zero hits after the edits.
   Historical references in `plans/impl/` and `command-findings/` are
   acceptable and expected (they document past state).
3. `ls cli/tests/fixtures/` -- the `nut/` directory must be absent.
4. `git status` -- the changeset is exactly: one modified Rust source
   file (`cli/src/parse/upsc.rs`), three deleted fixture files (and one
   removed directory).

## Risk

Low. The parser smoke tests assert structural properties
(`status_flags.contains(...)`, `charge_pct == Some(N)`,
`device.model == "Back-UPS ES 550G"`), which the captured fixtures
satisfy. The captured fixtures carry strictly more `driver.*` and
`battery.*` metadata than the legacy ones; nothing the parser tests
check depends on a field that is present in the legacy fixture but
missing in the captured one.

The captured `upsc-daemon-down.stderr` is byte-identical to the legacy
one across both stable and unstable lanes (single line:
`Error: Connection failure: Connection refused\n`). Deletion of the
legacy copy changes no parse contract.
