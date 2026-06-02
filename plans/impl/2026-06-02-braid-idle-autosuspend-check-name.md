# Fix: autosuspend check name in `braid idle` docs (`BraidIdle` -> `BraidPool`)

## Context

The `braid idle` doc page shows an example autosuspend check section named
`[check.BraidIdle]`, but braid's NixOS module generates the check under the
name `BraidPool`. The names never matched: `BraidPool` has been the module's
name since auto-suspend was introduced (`60c2fb4d`), and `BraidIdle` has only
ever existed in this one doc line (introduced in the manual-creation commit
`36541831`). This is an original inconsistency, not a rename the doc missed.

Impact: a reader who copies the example, then greps their generated
`/etc/autosuspend.conf`, inspects `services.autosuspend.checks`, or reads
autosuspend logs, sees `BraidPool` and cannot connect it back to the doc. The
page's own parenthetical at line 82 ("the in-tree module form omits it")
explicitly points the reader at the module section -- but that section has a
different name than the example shows, so the cross-reference is incoherent.

Intended outcome: the documented example name matches the name braid actually
emits, so the example, the line-82 module cross-reference, and the reader's
real system all line up.

## Fix direction (doc -> code, not code -> reverse)

Make the doc match the shipped code, not vice versa. `BraidPool` is the
authoritative, defensible name:

- It is what the module emits (`modules/braid/auto-suspend.nix:78`), what both
  VM tests assert (`tests/module/braid-auto-suspend.py:55`,
  `tests/module/braid-auto-suspend.nix:4`), and what every plan references.
- It protects the *pool* and pairs with its sibling check `BraidWol`
  (the WoL-readiness gate); `BraidPool`/`BraidWol` is a coherent pair.
- Renaming the module to `BraidIdle` instead would touch the module, two VM
  test files, and contradict the established name -- a much larger change to
  adopt the worse name. Not warranted.

`BraidIdle` is a single outlier string in the whole repo; the example body is a
deliberately simplified *manual* form (bare `braid idle`/`timeout`, plus
`enabled = true`, which the module omits and line 82 already explains). The
section name is the only accidental divergence, so a one-line header rename
fully closes the gap.

## Change

Single-line edit in `docs/commands/idle.md`:

- Line 74: `[check.BraidIdle]` -> `[check.BraidPool]`

No other edits. The surrounding example body and the line-82 explanation of the
manual-vs-module `enabled` difference stay as-is; renaming the header is what
makes that explanation coherent.

## Blast radius (verified)

- `rg 'BraidIdle'` over the entire repo returns exactly one hit:
  `docs/commands/idle.md:74`. Nothing else references it -- no code, tests,
  README, or other docs.
- `rg '\[check\.' docs/` returns only `docs/commands/idle.md:74`; no other doc
  carries a parallel autosuspend check-section example needing the same fix.
- Other autosuspend-mentioning docs (`guides/power-management.md`,
  `design/decisions/016-auto-suspend.md`, etc.) describe auto-suspend at a
  higher level and do not name the check section, so they are unaffected.

## Verification

- `rg -n 'BraidIdle' .` -> no results (the outlier is gone).
- `rg -n 'BraidPool' docs/commands/idle.md` -> the example header now matches
  `modules/braid/auto-suspend.nix:78` and the VM-test assertions in
  `tests/module/braid-auto-suspend.py`.
- Optional doc-build sanity: `mdbook build docs` still succeeds (this edit
  touches no cross-links, so linkcheck is unaffected).
- No code, no tests, and no other docs change, so no VM/Rust/parser test run is
  required for this edit.
