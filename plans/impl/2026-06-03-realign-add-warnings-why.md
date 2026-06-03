# Realign braid-add-warnings.nix `Why:` with the `[warn]` contract

## Context

This is the deferred Follow Up from the degraded-add-skip-balance impl
(`plans/impl/2026-06-03-degraded-add-skip-balance.md`). While rewriting
the `braid-add-warnings.py` real-run phase, the `.py` assertions were
updated to pin the `[warn]` contract, but the `.nix` header `Why:`
paragraph was left describing a stale, mid-migration design.

The drift: the `Why:` paragraph (lines 12-16) claims real-run

> must preserve today's `warning: pool has ...` stderr wording
> byte-identically so log scrapers do not drift.

That contract does not exist. PR 7 unified the rendering and deleted the
legacy `warning:` prefix outright. Both modes now render the same
canonical `[warn] pool has ...` body; only the stream differs (dry-run to
stdout via `Preview::render`, real-run to stderr via
`preview::render_notes_for_stderr`). Evidence:

- `cli/src/add.rs#format_add_missing_devices_warning` doc comment:
  "Returns the missing-devices warning body (no legacy `warning:`
  prefix)." No `warning: pool has` literal exists on the add path.
- The `.py` it heads asserts the opposite of its own `Why:`:
  `braid-add-warnings.py:99` requires the canonical `[warn] pool has 1
  missing device ...` line in real-run stderr, and `:126` / `:230` both
  assert `"warning: pool has" not in err`.

Impact: comment-only, zero behavioral effect, the test passes. But it is
actively misleading -- a reader trusting the header could "fix" a future
regression in the wrong direction (re-introducing `warning:`) or write a
new assertion contradicting the three live ones. The `What:` paragraph
(lines 3-10) is accurate; only the `Why:` second sentence drifts.

## Change

Single file: `tests/cli/braid-add-warnings.nix`, the `Why:` paragraph.

Replace lines 12-16:

```
# Why: PR 7 moved `eprintln!("warning: pool has N missing device...")`
# from a raw stderr write into `plan.notes`. Dry-run must emit the
# canonical `[warn] pool has ...` body-only form on stdout; real-run
# must preserve today's `warning: pool has ...` stderr wording
# byte-identically so log scrapers do not drift.
```

with:

```
# Why: PR 7 moved `eprintln!("warning: pool has N missing device...")`
# from a raw stderr write into `plan.notes` and dropped the legacy
# `warning:` prefix entirely. Both modes now wrap the same warning body
# via the same `status_line(StatusTag::Warn, ...)` helper and render it
# as `[warn] pool has ...`; only the stream differs -- dry-run to stdout
# (`Preview::render`), real-run to stderr
# (`preview::render_notes_for_stderr`). No legacy `warning:` wording
# survives on either stream; the `.py` asserts `warning: pool has` is
# absent from both the real-run and refusal-path stderr.
```

Why drop "byte-identical contract" from the replacement: the stale text
it removes earned its imprecision by attaching "byte-identical" to the
wrong thing (legacy-wording preservation). No test pins *cross-mode*
byte-identity -- Phase 2 checks a `[warn] pool has 1 missing device`
prefix substring on stdout while Phase 3/4 check the full line on
stderr; the shared output is a code property (both modes funnel
`PreviewNote::Warn` through `status_line(StatusTag::Warn, color, msg)`
in `cli/src/preview.rs`), not a directly-asserted one. Naming the helper
states the real mechanism without re-seeding the same ambiguity.

Constraints honored: ASCII `--` (not em-dash); code refs as bare
`path#symbol`-style spans; no line numbers in the prose.

## Verification

- Comment-only edit to a NixOS test header -- no Rust, no test logic, no
  behavioral surface. The VM suite has nothing new to assert; do not
  re-run it for this.
- Confirm the new prose matches the live assertions it documents:
  `rg -n "warning: pool has" tests/cli/braid-add-warnings.py` still shows
  the two `not in err` guards, and `braid-add-warnings.py` Phase 3 still
  asserts the `[warn] pool has 1 missing device` line is present.
- Sanity-check no stray em-dash glyph slipped in -- expect zero output,
  matching the glyph directly: `rg -n "—" tests/cli/braid-add-warnings.nix`
  (or `rg -nP "[\x{2012}-\x{2015}]" tests/cli/braid-add-warnings.nix` for
  the whole dash family). The earlier `[^-]--[^-]` form cannot do this --
  it matches ASCII `--`, never the `—` glyph (U+2014).
- Separately, `rg -n "warning:" tests/cli/braid-add-warnings.nix` WILL
  match and that is expected -- the new prose intentionally contains
  `` `warning:` `` in backticks. Those hits want an eyeball (correct
  backtick framing), not removal. Then read the edited header back.
