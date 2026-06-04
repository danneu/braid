# Fix the `ups status --json` "trusted healthy" contract (docs + pin tests)

## Context

`braid ups status --json` emits a success body that is the serialized
`UpscOutput` with `status_flags` always present. The only success-body
sentinel is `"warning": "ups_status_empty"`, which `JsonSuccessReport::new`
(`cli/src/ups.rs`) fires **solely** when `status_flags.is_empty()`.

`docs/commands/ups-status.md` currently tells scripts:

> "A trusted healthy success is a reachable UPS with a populated
> `status_flags` array and no top-level `error` or `warning` field."

That positive claim is wrong. Three recognized states serialize as
no-`error`/no-`warning` success bodies and would be read as "trusted
healthy" under that rule:

- `[OB]` -- on battery (every power outage).
- `[OB, LB]` -- low battery, **host shutdown imminent**.
- `[Unknown("WEIRD")]` -- an all-unrecognized status set braid cannot
  classify.

This is authority drift, not a code defect. braid's own mutation
preflight (`cli/src/preflight.rs#check_ups_not_on_battery`) already
**refuses** to start on every one of those states because it requires
affirmative `OL` via `UpscOutput::reports_utility_power()`
(`cli/src/parse/types.rs`). ADR 020
(`docs/design/decisions/020-ups-integration.md`, `status: Active`) and the
UPS guide (`docs/guides/ups.md`) both already frame the contract correctly
-- the commands page diverged from its own governing ADR.

Intended outcome: the commands page states the same trust criterion braid
uses everywhere (`OL` present, no blocker), the JSON body's meaning is
disentangled from UPS health, and the behavioral claims are pinned by
cheap, structure-insensitive tests.

## Decision

Docs-only reframe plus pinning unit tests. **No production code or JSON
shape change**, and no new `warning` sentinel.

Rationale (why not a sentinel): `status_flags` is already in the body and
is the authoritative input to braid's single trust classifier
(`reports_utility_power` / `is_critical`, shared by preflight and the TUI).
A `--json`-only `ups_not_online` sentinel would fork that classifier into a
third surface to keep in sync, re-encode a fact the flags already carry,
and fire a "warning" on normal outages while still not freeing scripts from
reading flags (OB-vs-LB still matters). The empty-status warning stays
narrow on purpose: empty is the one case with no flags to read.

## Changes

### A. `docs/commands/ups-status.md` -- the substantive reframe

1. **Replace the "trusted healthy" sentence** (currently the paragraph
   right after "Emits the serialized `UpscOutput` model.", before the
   `Shape:` example). New framing, in braid's `--`/ASCII CLI style:

   - A success body (no top-level `error`) is **trustworthy telemetry**:
     braid faithfully serialized whatever `upsc` reported. It is **not** a
     claim that the UPS is online.
   - On-battery (`OB`), low-battery (`OB LB`), and all-unrecognized status
     sets are all success bodies with no `error` and no `warning`.
   - To judge UPS state, read `status_flags`: utility power is proven only
     by the presence of `OL` with no blocking flag (`OB`, `LB`,
     `TESTFAIL`, `COMMBAD`, `FSD`) -- the same affirmative-`OL` criterion
     braid's own mutation preflight uses.
   - Cross-link the authoritative criterion:
     `[the UPS guide](../guides/ups.md#mutation-refusal-when-utility-power-is-not-verified)`
     (heading exists at `docs/guides/ups.md` "## Mutation refusal when
     utility power is not verified"; validated by `mdbook-linkcheck2`).

2. **Add the converse caveat** near the existing sentence "If `error` or
   `warning` is present, do not treat the typed body as healthy UPS state."
   Keep that sentence (correct, negative direction) and append: the
   converse does not hold -- the absence of `error` and `warning` does not
   by itself mean the UPS is online; inspect `status_flags` as above. Note
   that `ups_status_empty` fires only when `ups.status` is empty/missing
   (no flags to read), and is not a general health signal.

3. Leave the field-presence paragraph (null-vs-omitted) and the sentinel
   table rows unchanged -- they are factually correct.

### B. `cli/src/ups.rs` -- pin the behavioral claims (tests only)

These pin the exact claims the reframed doc makes about existing behavior,
asserting on serialized JSON (`value.get("warning")`), so they are
behavioral and structure-insensitive. Each gets the repo's
Intent/Why/Scenario preamble.

1. **New** `json_all_unknown_status_has_no_warning`: build from raw `upsc`
   text **through the real parser** -- `parse_upsc("ups.status: WEIRD\n")`
   (or the existing `parse_fixture` helper, which wraps it) -- then
   serialize via `JsonReport::success(&parsed)` (the real CLI path) and
   assert `value.get("warning").is_none()`, `value.get("error").is_none()`,
   and `assert_eq!(value["status_flags"], serde_json::json!(["WEIRD"]))`
   (compare `Value` to `Value` -- a bare `== ["WEIRD"]` does not compile;
   `serde_json::Value` has no `PartialEq` against a Rust array). Driving the
   parser instead of
   hand-building `UpscOutput` is deliberate: the user-facing scenario is
   `upsc` emitting an all-unrecognized `ups.status`, so the test must cover
   `parse_upsc` too. A future parser regression that dropped unknown-only
   tokens would make real `--json` emit `ups_status_empty`; a hand-built
   struct would hide that, raw input catches it. (Parser confirmed: a lone
   unrecognized token maps to `UpsStatusFlag::Unknown` via
   `cli/src/parse/upsc.rs#parse_upsc`, per the existing `NEWFLAG` test.)
   Fills the confirmed coverage gap and locks in that an all-unknown body
   is a no-warning success.
2. **Strengthen** `json_onbattery_fixture_has_expected_shape` and
   `json_lowbattery_fixture_has_expected_shape` (already serialize the
   OB and OB-LB fixtures): add one line each --
   `assert!(value.get("warning").is_none(), "got: {value}")` -- pinning
   that the two most common "not online" states are no-warning bodies.

### C. `docs/guides/ups.md` -- small consolidation (optional, in-scope)

The guide's JSON-output table (the "Checking status" section) has a generic
`UPS reachable` row and omits the `ups_status_empty` warning row the
commands page carries. Just adding the warning row would leave the generic
row still reading as covering the empty case, so make the guide table
mirror the commands page exactly:

- **Edit** the existing row `| UPS reachable | serialized UpscOutput | 0 |`
  to `| UPS reachable with populated ups.status | serialized UpscOutput | 0 |`
  so it no longer also reads as the empty case.
- **Add** the warning row immediately after it:
  `| UPS reachable but ups.status empty | serialized UpscOutput plus "warning": "ups_status_empty" | 0 |`

### D. No change (state explicitly, do not touch)

- `cli/src/ups.rs` production code (`JsonSuccessReport::new` keeps the
  `is_empty()` trigger).
- `cli/src/parse/types.rs`, `cli/src/preflight.rs` -- already correct.
- `cli/src/doctor.rs` -- its `ups_daemon` check returns "reachable" for an
  all-unknown body, which is correct: that check is scoped to reachability
  (guide.md says so), not health. **Not** a sibling bug.
- ADR 020 -- already states the contract correctly; it is the authority we
  align the commands page to. No rewrite (also a frozen-doc-hygiene
  concern, though 020 is Active).

## Explicitly out of scope

- No new `JsonWarning` variant / JSON sentinel for not-online or
  unrecognized bodies (forks braid's single classifier; see Decision).
- No new VM dummy-ups `.dev` fixture. The VM test
  (`tests/cli/braid-status-ups.py`) already covers healthy-no-warning and
  empty-warning; a unit test driving the real `parse_upsc` ->
  `JsonReport::success` path covers the all-unknown case at the right
  altitude without booting a VM.

## Files touched

- `docs/commands/ups-status.md` (reframe + cross-link)
- `cli/src/ups.rs` (one new test, two one-line assertions)
- `docs/guides/ups.md` (one table row edited + one added) -- optional consolidation

## Verification

1. `just test-rust` -- expect `json_all_unknown_status_has_no_warning` and
   the strengthened onbattery/lowbattery tests to pass; whole `ups::tests`
   module green. (Crate is `braid-cli`; `just test-rust` wraps it.)
2. `mdbook build docs` -- confirms the new
   `../guides/ups.md#mutation-refusal-when-utility-power-is-not-verified`
   cross-link resolves under `mdbook-linkcheck2` (a broken link fails CI).
3. No VM run needed: production code is unchanged, so
   `tests/cli/braid-status-ups.py` behavior is unaffected. Mention to the
   user that the focused `just test-rust` + `mdbook build docs` cover this
   change; the full VM suite is not warranted for a docs+test edit.
4. Manual read-through: a script following the reframed page uses
   `"OL" in status_flags and no blocker`, matching
   `preflight.rs#check_ups_not_on_battery` and `guides/ups.md`.

## Implementation notes

- Change A.2 (converse caveat): placed the caveat as a separate paragraph
  immediately *after* the existing "If `error` or `warning` is present..."
  paragraph, rather than inserting it mid-paragraph. Inserting between that
  sentence and "For these cases, `--json` writes only to stdout..." would
  have orphaned the "these cases" antecedent (the error/warning cases). A
  following paragraph keeps the caveat adjacent without disturbing the
  existing prose.
- Change C (guide table rows): the plan's literal row text omitted
  backticks, but the directive "make the guide table mirror the commands
  page exactly" governs -- the commands page and the existing guide rows
  both backtick `ups.status`, `UpscOutput`, and the JSON literal, so the
  two edited/added rows use backticks to match.
