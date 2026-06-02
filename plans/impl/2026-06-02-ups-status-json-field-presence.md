# Plan: document the `--json` field-presence contract for `braid ups status`

## Context

`docs/commands/ups-status.md` documents `braid ups status --json` as a
"stable shape for scripts" but shows only a fully-populated example body
(lines 42-70). It never states the actual serialization contract: every
typed field is **always present**, and a field the driver did not report
serializes as `null` -- the key is never omitted.

This matters because the `--json` surface targets script authors, and
NUT drivers vary widely in which keys they publish (see the parser
comment at `cli/src/parse/types.rs#BatteryFields`: "Every field is
`Option` because NUT drivers vary widely in which keys they publish").
Partial output is the common case. A reader who only sees the populated
example will write `has("charge_pct")` guards or treat a missing key as
the absence signal, then break when a thin driver yields
`"charge_pct": null` -- the key is present, the value is `null`.

The behavior is correct and is the *better* contract (a stable,
all-keys-present shape is friendlier to schema-based consumers than one
that varies its key set per driver). So the fix is to **document and
enforce** the existing behavior, not change it.

### Why this shape (decisions baked in)

- **Docs + a contract test, not docs alone.** braid guards every
  documented JSON/parser surface with tests and warns against
  documenting an invariant nothing enforces (AGENTS.md "Doc Comments").
  The contract this doc will assert is currently **unguarded**: a future
  `#[serde(skip_serializing_if = "Option::is_none")]` on the parse model
  would switch to omitting absent keys and silently falsify the doc
  while every existing shape test still passes (none of them assert on
  an absent field). A snapshot test closes that gap in a dozen lines
  plus a committed `.snap`.
- **Prose only, no second sparse example in the docs.** braid reference
  docs are terse and drift-averse (cross-links are CI-linted precisely
  to kill stale pointers), and a second hand-maintained ~28-line JSON
  body *in the page* has no linter to catch divergence from real serde
  output. The sparse `null`-shaped JSON body still exists -- but as the
  contract test's committed snapshot (Change 2), where `cargo test`
  enforces it against drift. Rule (prose) + existing populated example +
  enforced snapshot is complete without a rottable fourth artifact in
  the docs.

## Change 1 -- doc note (`docs/commands/ups-status.md`)

Insert one interpretive paragraph immediately after the example block
(after the closing ```` ``` ```` on line 70), as the first of the
"how to read this shape" notes -- ahead of the existing `status_flags`
ordering note -- so the caveat sits attached to the example a reader
copies from. Proposed text:

> In a success body (the shape above -- a reachable UPS, no top-level
> `error`), every typed field is always present: a scalar the driver did
> not report serializes as `null` rather than being omitted, and the
> `battery`, `input`, and `device` objects are always present even when
> all of their fields are `null`. Test typed fields for a `null` value,
> not for a missing key -- a `has(...)` check on any typed key always
> returns true. `status_flags` and `extra` are always present but never
> `null` (`[]` and `{}` when empty). The only field omitted when absent
> is the top-level `warning` (see the table below). Error bodies are the
> exception -- they carry `error`/`detail` and none of the typed keys, so
> a script must confirm there is no top-level `error` before relying on
> the rule above.

Every clause is verifiable against the code:

- Scalars (`load_pct`, `realpower_nominal_watts`, `test_result`) are
  `Option<T>` with no `skip_serializing_if` -> serde emits `null`.
- `battery`/`input`/`device` are **non-`Option`** nested structs
  (`cli/src/parse/types.rs#UpscOutput`) -> always present as objects,
  with `null` inner fields when empty.
- `extra` is a `BTreeMap` -> always present, `{}` when empty.
- `warning` is the lone `#[serde(skip_serializing_if)]` field
  (`cli/src/ups.rs#JsonSuccessReport`) -> omitted when absent.

No parallel edit is needed in `docs/guides/ups.md`: its `--json` section
describes "the full parsed model" and shows only the error-sentinel
table -- it does not enumerate fields, so the per-field rule has exactly
one home.

## Change 2 -- contract test (`cli/src/ups.rs`)

Add one focused **snapshot** test in the existing `#[cfg(test)] mod
tests`, named in the `snapshot_json_*` family alongside
`cli/src/ups.rs#snapshot_json_query_failed`. Build the value with the
hand-built sparse pattern from
`cli/src/ups.rs#json_output_has_status_and_battery_keys`
(`UpscOutput { .. }` / `BatteryFields::default()` -- no fixture), then
snapshot it through the module's existing `snap_json!` macro (which
wraps `insta::assert_json_snapshot!`):

```rust
let parsed = UpscOutput {
    status_flags: vec![UpsStatusFlag::Ol], // non-empty => no warning
    battery: BatteryFields::default(),
    load_pct: None,
    realpower_nominal_watts: None,
    input: InputFields::default(),
    test_result: None,
    device: DeviceFields::default(),
    extra: std::collections::BTreeMap::new(),
};
snap_json!(&JsonReport::success(&parsed));
```

Per braid test conventions it gets its own Intent / Why it exists /
Scenario preamble; the "Why" names the regression guarded -- a future
`#[serde(skip_serializing_if = "Option::is_none")]` on the parse model
silently switching to omitted keys.

Why a snapshot, and why it is *safe* here: the committed `.snap` pins
**every** key's presence plus its `null` / `{}` / `[]` shape in a single
artifact, so it covers all ~14 nullable fields at once and automatically
extends to any field added later -- a strict superset of enumerating
`contains_key` assertions, at fewer lines. A missing key simply vanishes
from the snapshot text and surfaces as a diff, so the snapshot catches
an omitted-key regression directly and sidesteps the
`serde_json::Value` `Index`-returns-`Null` gotcha that a hand-rolled
`is_null()` assertion would fall into. Because `status_flags` is
non-empty, the snapshot also demonstrates the other half of the
`warning` contract -- `warning` is *absent* (omitted) here, while
`cli/src/ups.rs#json_output_with_empty_status_has_warning_and_body`
already pins the present case.

The sibling `cli/src/ups.rs#json_online_fixture_has_expected_shape`
deliberately uses structural assertions instead of a snapshot, but for a
reason that does not apply here: it parses a *captured* fixture whose
`extra` map carries `driver.*` keys that bump every nixpkgs revision
(see its in-test comment). This value is hand-built with an **empty**
`extra`, so its serialization is fully deterministic and a snapshot is
the right tool.

## Files

- `docs/commands/ups-status.md` -- add the field-presence paragraph after
  the example block.
- `cli/src/ups.rs` -- add the contract test in `mod tests`.

Reuse, do not recreate:

- Test construction pattern: `cli/src/ups.rs#json_output_has_status_and_battery_keys`.
- Snapshot macro: `cli/src/ups.rs#snap_json` (wraps
  `insta::assert_json_snapshot!`); the new `.snap` lands in
  `cli/src/snapshots/` next to `snapshot_json_query_failed.snap`.
- Struct definitions / serde attrs being pinned:
  `cli/src/parse/types.rs#UpscOutput`, `#BatteryFields`, `#InputFields`,
  `#DeviceFields`; `cli/src/ups.rs#JsonSuccessReport` (the `warning` field).

## Verification

1. `just test-rust` -- on first run the new snapshot test writes a
   `.new` snapshot; review and accept it (`cargo insta review` /
   `cargo insta accept`), inspect that the accepted `.snap` shows every
   typed key present with `null` / `{}` / `[]` values and no `warning`,
   and commit the `.snap`. Confirm it is real coverage by temporarily
   adding `#[serde(skip_serializing_if = "Option::is_none")]` to e.g.
   `UpscOutput::test_result`, re-running, and confirming the snapshot
   **diffs** (the `test_result` key vanishes), then revert.
2. `mdbook build docs` -- the edited page renders and linkcheck passes
   (no new or broken cross-links introduced).
3. Eyeball: `docs/commands/ups-status.md` reads cleanly -- populated
   example, then the new field-presence note, then the existing
   `status_flags` / `extra` notes and the sentinel table.

Out of scope (no fixture/VM churn): no new `upsc` fixture is added, so
`just capture-all-fixtures` / `just test-parsers` are not implicated.
This is a parser-behavior *documentation + unit test* change, not a
parser-critical tool-version event.
