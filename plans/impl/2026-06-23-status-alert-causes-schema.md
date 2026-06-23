# Plan: document the two missing `alert_causes` cause objects in `status.md`

## Context

`braid status --json` emits `alert_causes` as the raw `Vec<AlertCause>`
(`cli/src/status.rs:387`). `AlertCause` is an internally-tagged
(`#[serde(tag = "type", rename_all = "snake_case")]`) enum with **6** variants
(`cli/src/alert.rs#AlertCause`), so the `type` discriminator plus its fields are
the entire wire contract a consumer parses.

The `--json` field reference at `docs/commands/status.md:514`-`520` documents only
**4** of them: `btrfs_device_errors`, `missing_device`, `smartd_alert`,
`computation_error`. Two are missing:

- `scrub_failed` -- added in `d6e6de18`
- `enospc_risk` (carries three numeric fields `margin`/`count_below`/`device_count`
  that no documented cause has) -- added in `cf28ce7f`

Both introducing commits edited `status.md` elsewhere (the human banner prose and
the on-demand ENOSPC advisory) but never the `--json` bullet list, so the schema
reference silently drifted. A monitoring tool that builds its parser from the
documented schema hits an undocumented `{"type":"scrub_failed"}` or
`{"type":"enospc_risk",...}` object and either drops the alert or fails -- and it
cannot infer `enospc_risk`'s shape, since three numeric fields appear on no other
documented cause.

**Outcome:** the `alert_causes` schema reference enumerates all 6 cause objects
with their exact serialized shapes (so a downstream parser is complete), the
human-banner and ADR-014 surfaces agree with `format_status_human`, and a new
regression guard fails CI if a future cause variant is added or renamed without a
doc update -- so this exact drift cannot recur a third time.

## Authoritative sources (copy shapes/wording from these; do not invent)

- Exact serialized shapes are pinned by tests -- match them verbatim:
  - `cli/src/alert.rs#scrub_failed_latch_roundtrip_and_json_shape` -> `{"type":"scrub_failed"}` (no fields)
  - `cli/src/alert.rs#enospc_risk_latch_roundtrip_and_json_shape` -> `{"type":"enospc_risk","margin":-1063256064,"count_below":1,"device_count":2}`
- Canonical prose for each variant: `docs/design/decisions/014-alerts.md` (the
  complete, correct 6-variant list -- `ScrubFailed` and `EnospcRisk` descriptions).
  ADR 014 is the authority; `status.md` is the end-user-facing duplicate that fell
  behind. Keep `status.md` self-contained (do not replace the list with a link to
  the ADR -- the command reference owns the JSON schema for end users).
- Exact human-banner strings and the two-tier (NOTICE vs ALERT) logic:
  `cli/src/status.rs#format_status_human` (the rendering) and
  `cli/src/status.rs#status_human_warning_only_renders_notice_not_alert` (the
  pinning test). Copy banner/cause-line text from these verbatim.

## Change 1 (required): complete the `alert_causes` bullet list

File: `docs/commands/status.md`, the list at lines 514-520.

Add two bullets, ordered to match the enum declaration order
(`...smartd_alert, scrub_failed, computation_error, enospc_risk`). Match the
existing bullets' style exactly: `{ "type": ..., "field": <number> }` form,
`<number>`/`"<string>"` placeholders, two-space indent, `--` dash, terse one-line
description.

- Insert **after** the `smartd_alert` bullet (line 518):

  > `{ "type": "scrub_failed" }` -- the scheduled maintenance scrub
  > (`braid-scrub.service`) failed to run or complete. No payload fields.

- Append **after** the `computation_error` bullet (line 520), as the last bullet:

  > `{ "type": "enospc_risk", "margin": <number>, "count_below": <number>,
  > "device_count": <number> }` -- the pool is one disk-loss away from RAID1
  > chunk-pair ENOSPC. `margin` is the signed risk magnitude (negative = at-risk
  > depth); `count_below` of `device_count` devices sit below the per-device
  > unallocated threshold. This is the only Warning-tier (non-beeping) cause; the
  > others are Critical.

  Rationale for the severity aside: severity is *not* a JSON field (it is computed,
  not serialized), but the Warning-vs-Critical distinction is the one behavioral
  fact a consumer most needs and cannot read from the payload. Frame it as
  behavior, as above -- do **not** add a `severity` key to the shape.

## Change 2 (recommended, same root cause): document the two-tier banner

File: `docs/commands/status.md`, the "Alert banner" section (lines 78-89).

The section currently shows **only** the Critical banner
(`ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and
silence.`) and a prose summary that omits `enospc_risk`. But `enospc_risk` is the
single Warning-tier cause, and a Warning-*only* state renders a **different**
banner and cause line (`cli/src/status.rs#format_status_human`, lines ~1420-1458):

```
NOTICE -- capacity risk detected. Run 'braid ack' to acknowledge.
  - ENOSPC risk: pool is one disk-loss from being unable to restore RAID1 redundancy
```

So merely name-dropping `enospc_risk` in the existing prose would be **wrong** --
it implies ENOSPC appears under the "ALERT ... silence" banner. Instead:

1. Keep the existing `ALERT` banner example for Critical causes.
2. Add a second fenced example showing the Warning-only `NOTICE` banner and the
   ENOSPC cause line above (copy both strings verbatim from
   `cli/src/status.rs#format_status_human`; note "acknowledge" with no "silence",
   reflecting the non-beeping tier).
3. Add a sentence on **when** each renders: the banner reflects the highest cause
   severity (`AlertState::severity` / `.max()`), so an ENOSPC-risk-only state shows
   `NOTICE`, but any Critical cause present -- even alongside ENOSPC risk -- keeps
   the `ALERT` banner. This is pinned by
   `cli/src/status.rs#status_human_warning_only_renders_notice_not_alert` and the
   adjacent mixed-severity / scrub-failure banner tests.
4. Drop the inline `(`--json` cause value `{"type":"scrub_failed"}`)` parenthetical
   from the line-89 prose sentence. After Change 1 the JSON list is the canonical
   home for wire values, so `scrub_failed` should no longer be the lone cause whose
   serialized value is quoted in the human-banner prose -- the asymmetry reads as a
   leftover stopgap. No information is lost; the value now lives in the Change 1
   bullet.

Leave `computation_error` out of the human prose -- it is an internal fail-closed
cause, not an operator condition, and "Alert causes include ..." already reads as
non-exhaustive.

## Change 3 (required -- authority correctness): fix ADR 014's stale banner sentence

Priority note: unlike Changes 1-2 (which fix *incomplete* end-user docs), this
fixes a sentence that is *wrong* -- ADR 014 is the authority doc and line 33
currently contradicts line ~100 of the same ADR and `format_status_human`. A
self-contradicting authority outranks an incomplete reference, so if scope is ever
cut, this ranks above Change 2.

File: `docs/design/decisions/014-alerts.md`, line 33. It currently reads:

> The status banner is cause-neutral ("disk health issue detected"); cause details
> appear below it and in JSON output.

This predates the severity tier (`cf28ce7f`) and now contradicts both
`cli/src/status.rs#format_status_human` and ADR 014's own later section
`#severity-tiers-and-the-enospc-baseline` (line ~100, "a distinct lower-urgency
banner for a Warning-only alert"). The ADR's cause list (lines 25-31) is correct
and complete; only this one banner sentence fell behind its own document.

Rewrite line 33 to reflect the two-tier behavior: Critical causes render the
cause-neutral `ALERT` banner ("disk health issue detected ... acknowledge and
silence"), while a Warning-only `EnospcRisk` state renders the lower-urgency
`NOTICE` capacity banner ("capacity risk detected ... acknowledge"); cause details
still appear below the banner and in JSON. Keep it terse -- one or two sentences,
deferring the full rationale to the severity-tiers section it should cross-link.

Punctuation: match ADR 014's existing conventions -- the file's prose uses Unicode
em-dashes, and it is a design doc, **not** CLI output, so the ASCII-only output
rule in AGENTS.md does not apply here. Do not reflow the file to ASCII.

## Change 4 (required): recurrence guard pinning the cause set against the docs

This is the change that stops the bug from coming back. The `type` set drifted
undocumented **twice** (the `scrub_failed` and `enospc_risk` commits, per Context)
because nothing pins the `alert_causes` element schema against the docs. The
project already establishes exactly this convention one level up:
`cli/src/status.rs#mounted_status_envelope_top_level_keys_are_pinned` pins the
`StatusReport` top-level key set with the comment "the docs/commands/status.md JSON
section is a hand-maintained mirror ...; five fields drifted undocumented ... On
failure, update BOTH this set and the docs/commands/status.md JSON output section."
But that test pins only the *top-level* keys -- it never descends into the
`alert_causes` element, which is the exact gap that let the discriminators drift.
(The project also ships nine `scripts/docs/check-*.py` parity guards, e.g.
`check-doctor-table-parity.py`, so a docs/code-sync guard is established practice,
not over-engineering.)

Add a regression test, beside the `*_latch_roundtrip_and_json_shape` tests in
`cli/src/alert.rs` (their natural home -- they already serialize `AlertState`):

- Enumerate every variant via an **exhaustive `match` over `AlertCause` with no
  `_` arm**, constructing one value per variant. The missing-arm compile error is
  the forcing function: a future variant cannot be added without updating this
  match, which forces adding its `type` string, which forces the doc assertion.
- For each, derive the serialized discriminator from serde (serialize the value,
  read its `type` field) rather than hardcoding the literal -- so a `rename_all`
  change is caught too, not just a new variant.
- Assert `include_str!` of `docs/commands/status.md` (path relative to the source
  file -- confirm the `../../docs/...` depth) contains each `type` string.
- Carry a comment mirroring the envelope test's: "On failure, update BOTH this
  match and the docs/commands/status.md `alert_causes` cause list."

This is behavioral (fires exactly when a wire discriminator is added or renamed
without a doc update -- a real contract change) and structure-insensitive (asserts
token presence, not bullet formatting/order). Preferred over a new
`scripts/docs/check-*.py`: the compiler-forced match is more robust than
re-parsing the enum from text.

## Non-goals

- **No `severity` field** added to the JSON shape (see Change 1 rationale): severity
  is computed, not serialized.
- **No restructure** of `status.md` into a link to ADR 014 -- the command reference
  owns the JSON schema for end users.
- **No new `scripts/docs/` linter** for this: Change 4's in-crate compiler-forced
  match supersedes a text-parsing parity script (which was the genuinely
  over-engineered option). The two existing `*_latch_roundtrip_and_json_shape`
  tests remain the source the docs copy *shapes* from; Change 4 only adds the
  *completeness* guard they don't provide.

## Verification

1. `just test-rust` (or `cargo test -p` the CLI crate) -- the new Change 4 guard
   passes with all 6 `type` strings present, and **fails as expected** if you
   temporarily delete one cause bullet from `status.md` (confirm it fires for the
   right reason before relying on it).
2. `just docs-build` -- confirms mdBook builds and `mdbook-linkcheck2` passes (the
   new bullets add only inline code spans, no links, so no linkcheck risk).
3. Eyeball the rendered `alert_causes` list: 6 bullets, `type` values
   `btrfs_device_errors`, `missing_device`, `smartd_alert`, `scrub_failed`,
   `computation_error`, `enospc_risk`.
4. Cross-check the two new shapes character-for-character against the JSON strings
   asserted in `cli/src/alert.rs#scrub_failed_latch_roundtrip_and_json_shape` and
   `cli/src/alert.rs#enospc_risk_latch_roundtrip_and_json_shape`, and the prose
   against `docs/design/decisions/014-alerts.md` variant descriptions.
5. Cross-check the new `NOTICE` banner example (banner line + ENOSPC cause line)
   character-for-character against the strings in
   `cli/src/status.rs#format_status_human` and the assertions in
   `cli/src/status.rs#status_human_warning_only_renders_notice_not_alert`.
6. ADR 014 self-consistency: confirm the rewritten line-33 banner sentence agrees
   with the `#severity-tiers-and-the-enospc-baseline` section (~line 100) and with
   `cli/src/status.rs#format_status_human` -- no surface still claims the banner is
   unconditionally cause-neutral.
7. Per-file punctuation: `status.md` stays ASCII (`--`, plain quotes -- the docs
   ASCII linter does not cover `docs/`, but match the file's existing convention);
   `docs/design/decisions/014-alerts.md` keeps its existing Unicode em-dashes -- do
   not flip either file's convention.
