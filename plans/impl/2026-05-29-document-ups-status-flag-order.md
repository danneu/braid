# Document the UPS `status_flags` emission-order contract

## Context

A review finding proposed deleting the test
`json_output_status_flags_preserve_insertion_order`
(`cli/src/ups.rs:393`) as "dead weight" that "only proves serde
preserves `Vec` order" and "duplicates the parser-side order contract."

Investigation (git history of `bee9bb1` -> `988affd`) shows the finding
is **wrong**: this test is one of several coordinated regression guards
created by the emission-order pivot. Commit `bee9bb1` added a
`#[serde(serialize_with = "serialize_status_flags_sorted")]` hook that
lex-sorted flags **at this exact JSON boundary** (over a `HashSet`
store); commit `988affd` reversed that design -- switched storage to a
`Vec`, deleted the sort hook, and made `upsc` emission order the
script-facing contract across the parser, human CLI, `--json`, and both
TUI surfaces. The JSON guard catches a serialize-layer re-sort that the
parser-order test (`cli/src/parse/upsc.rs:189`) cannot, because the
parser test never serializes. The two are not duplicates.

The verify-issue conclusion was therefore **docs**, not delete. The
root cause of the bad finding: the load-bearing emission-order invariant
lives only in git history and `plans/impl/`, so its weakest restatement
(the JSON preamble) reads as vacuous. The intended outcome of this
change is to give the invariant one discoverable, authoritative home and
make the weak/ambiguous restatements consistent with it, so this class
of finding cannot recur.

This is **docs/comments plus one bridge test**. No production code,
existing-assertion, or public API changes; the single code addition is a
unit test (step 5) that makes the documented end-to-end TUI claim honest.

## What is wrong today

| Site | Problem |
| --- | --- |
| JSON test preamble `cli/src/ups.rs:388-392` | Says "a future array-level sort would diverge" but never names the real removed `serialize_with` hook, and does not distinguish itself from the parser-order test -- so it reads as vacuous. |
| `UpscOutput.status_flags` doc `cli/src/parse/types.rs:669-674` | Names only "human render and `--json`" -- omits both TUI surfaces (`format_ups_flags`, Browse) and the removed sort hook. Not yet the canonical home. |
| ADR `docs/design/decisions/020-ups-integration.md:57,61` | Line 57 calls it "parsed as a **set** of flags," which contradicts the order-preserving contract; line 61's `--json` paragraph never states emission order is preserved. A behavioral contract missing from the authority doc. |
| User docs `docs/commands/ups-status.md:94` | `--json` is "stable shape for scripts"; the example shows only single-flag `["OL"]`. Ambiguous about multi-flag ordering -- the exact thing scripts care about. |
| TUI bridge `cli/src/tui/probe.rs:764-771` | The single `UpscOutput -> UpsSnapshot` bridge has no order test: the existing probe test checks `.contains()` (membership) and the render tests seed flags manually. The plan's "TUI preserves emission order" claim is unpinned end-to-end. |

## The change

### 1. ADR 020 -- make `docs/design` the authority (required by AGENTS.md)

`docs/design/decisions/020-ups-integration.md`

- **Line 57**: replace "parsed as a set of flags ... not an enum" with an
  order-preserving framing:
  > `ups.status` is parsed into an ordered, deduplicated list of flags
  > (`OL`, `OB`, `LB`, `CHRG`, `DISCHRG`, `RB`, ...), not an enum. Flags
  > are stored in `upsc` emission order; membership and dedup give set
  > semantics without imposing a sort. Display severity is derived from
  > the combination; unknown tokens are preserved in the parsed model so
  > that new NUT statuses do not silently disappear.
- **Line 61** (`--json` shape paragraph): append one sentence:
  > The `status_flags` array preserves first-seen `ups.status` token order
  > across the human, `--json`, and TUI surfaces -- braid imposes no sort
  > of its own. Order is deterministic for a given UPS state (whitespace is
  > normalized and repeated tokens collapse to first-seen); it is not a
  > byte copy of the raw `ups.status:` line.

This is the single authoritative statement the code comments below point
to.

### 2. `status_flags` field doc -- the canonical code home

`cli/src/parse/types.rs:669-674`

Replace the current 5-line doc with one that names every surface and the
removed hook (intent/invariant/coupling, none recoverable from code):

> Flags from `ups.status`, in first-seen token order, deduplicated on
> push. That order is the script-facing contract (ADR 020): the human CLI
> (`format_status`), `--json`, the TUI bridge (`probe_ups_for_tui`), and
> both TUI renders (`format_ups_flags`, Browse) carry this `Vec` verbatim
> -- none re-sorts. The `--json` path once lex-sorted via a
> `serialize_with` hook; it was removed so every surface agrees.
> Membership tests treat the `Vec` as a set; dedupe-on-push keeps those
> calls honest.

### 3. JSON test preamble -- the cited fix

`cli/src/ups.rs:388-392`

Rewrite to name the regression and rebut the "duplicate" claim in-place:

> // Intent: --json serializes status_flags in stored Vec order, with no
> // re-sort at the serialization boundary.
> // Why it exists: this boundary once carried a `serialize_with` hook
> // that lex-sorted the flags; the emission-order pivot removed it and
> // made Vec order the contract (see status_flags doc + ADR 020). A
> // re-added serialize sort would pass the parser-order test, which never
> // serializes -- so this guard is not redundant with it. The 17-flag
> // fixture also pins every variant's NUT token verbatim.
> // Scenario: a UPS reporting every known flag at once plus an
> // unrecognized driver-extension token.

(Test body and assertion are unchanged.)

### 4. User docs -- one clarifying sentence

`docs/commands/ups-status.md` (after the JSON shape block, ~line 71)

> `status_flags` lists flags in first-seen `ups.status` token order
> (whitespace normalized, duplicate tokens dropped); braid does not sort
> them, so the order is deterministic for a given UPS state.

### 5. Bridge test -- make the TUI claim honest end-to-end

`cli/src/tui/probe.rs` (in the existing `probe_ups_for_tui` test module,
~line 3119)

The TUI render guards (`format_ups_flags_preserves_insertion_order`,
Browse snapshot) seed `snap.flags` manually, and the existing probe test
asserts membership with `.contains()`. So no test pins flag *order*
through `probe_ups_for_tui` -- the single `UpscOutput -> UpsSnapshot`
bridge (`cli/src/tui/probe.rs:764`), currently `flags:
parsed.status_flags.clone()` (line 771). Without this test, a future
sort inserted at the bridge would reorder both TUI surfaces while the
parser test, the render tests, and the JSON test all stay green -- and
the new docs would be a lie.

Add a focused guard mirroring the other four surfaces' naming:

> // Intent: probe_ups_for_tui carries parsed status_flags into the
> // snapshot in first-seen ups.status order, with no sort at the bridge.
> // Why it exists: this is the single UpscOutput -> UpsSnapshot bridge;
> // both TUI render guards seed flags manually and the other probe test
> // only checks membership, so a re-sort here would skip every existing
> // guard. Pins the bridge as the fifth no-resort surface (ADR 020).
> // Scenario: a UPS emitting CAL OL CHRG RB in that order.
> #[test]
> fn probe_ups_for_tui_preserves_status_flag_order() {
>     let mock = /* runner returning "ups.status: CAL OL CHRG RB\n" */;
>     let snap = probe_ups_for_tui(&mock, "ups");
>     assert_eq!(snap.flags, vec![Cal, Ol, Chrg, Rb]);
> }

Use the same mock-runner helper the sibling probe tests already use. The
bridge is a verbatim `.clone()` today, so this test passes immediately --
it is a regression guard, not a TDD red-first step.

## Out of scope (deliberate)

- **Sibling preambles** -- `parse_upsc_preserves_status_flag_order`
  (`cli/src/parse/upsc.rs:182`), `format_status_preserves_insertion_order`
  (`cli/src/ups.rs:318`), `format_ups_flags_preserves_insertion_order`
  (`cli/src/tui/view/mod.rs:2475`), and the Browse snapshot guard
  (`cli/src/tui/browse/view.rs:716`) are already adequate; the TUI
  Data-tab one is the quality template. Once steps 1-2 centralize the
  rationale, restating it in three more preambles adds churn without
  dissolving anything. Leave them.
- **No production code, existing-assertion, container, or API changes.**
  Emission-order behavior is already correct; the only code touched is the
  additive bridge guard (step 5). The JSON test body and assertion stay
  byte-identical -- only its preamble changes.

## Verification

1. `just test-rust` -- confirms the edited files compile and all UPS
   tests pass, including the new `probe_ups_for_tui_preserves_status_flag_order`
   guard (passes immediately; the bridge is a verbatim clone). The JSON
   test assertion is untouched, so its green run proves the preamble edit
   is purely cosmetic.
2. `mdbook build docs` -- validates ADR 020 and `ups-status.md` edits
   (mdbook-linkcheck runs here per AGENTS.md). No new cross-links are
   added, so this is a safety check.
3. No fixture refresh: no parser-critical tool version changed, so
   `just capture-all-fixtures` / `just test-parsers` are **not** required.

## Files modified

| File | Change |
| --- | --- |
| `docs/design/decisions/020-ups-integration.md` | Document emission-order contract; reconcile "set of flags" wording (lines 57, 61) |
| `cli/src/parse/types.rs` | Expand `status_flags` field doc to canonical home (lines 669-674) |
| `cli/src/ups.rs` | Rewrite `json_output_status_flags_preserve_insertion_order` preamble (lines 388-392) |
| `cli/src/tui/probe.rs` | Add `probe_ups_for_tui_preserves_status_flag_order` bridge guard (~line 3119) |
| `docs/commands/ups-status.md` | One sentence on multi-flag emission order (~line 71) |
