# Document the confirmation-only redundancy-warning convention in `remove`

## Context

A review finding claimed `braid remove`'s "no RAID1 redundancy" warning is
missing from `--dry-run`, calling it a Decision-022 violation. Investigation
showed the opposite: the warning is **confirmation-UI, gated behind
`!params.yes` by deliberate design**, and the sibling command `replace` keeps
the byte-for-byte analogous warning the same way -- documenting it in two doc
comments and guarding it with three regression tests that assert it must not
leak into dry-run.

The real defect is an asymmetry, not a bug:

- `replace.rs` documents the convention (struct doc `replace.rs:175-180`,
  `preview()` doc `replace.rs:395-397`) and guards it
  (`plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render`
  and two siblings, `replace.rs:4851-5037`).
- `remove.rs` has **neither** a comment explaining its `remove.rs:267-268`
  warning is confirmation-only, **nor** a test asserting its absence from
  dry-run.
- The cross-command classifier (which warnings are `PreviewNote`s vs
  confirmation-UI) is **not written down anywhere**, even though braid already
  applies it deliberately -- `add`'s keyfile-asymmetry warning and
  `remove_missing`'s ENOSPC soft-warn were both migrated *to* `PreviewNote`s,
  while `remove`/`replace`'s redundancy warning was deliberately *kept*
  confirmation-only.

This asymmetry is exactly what regenerates the finding: a reviewer lands on
`remove.rs:268`, checks dry-run, sees nothing, files a bug. The goal is to
bring `remove` to parity with `replace` and write the convention down once, so
the class of finding dissolves and a future agent cannot silently "fix" it by
adding the warning as a note.

Out of scope (decided with the user): the end-user doc
`docs/commands/remove.md:61` stays as-is -- "Warns when removal leaves a single
disk" is accurate for the default interactive run, and the dry-run-vs-confirm
mechanic does not belong in operator-facing docs.

## The convention being documented

- **`PreviewNote`** = a discovered precondition or diagnostic (busy-op,
  readonly-probe-fail, keyfile asymmetry, ENOSPC soft-warn). Renders
  byte-compatibly across dry-run stdout, real-run stderr, and the failure path.
- **Confirmation UI** = the interactive `!params.yes` block: the
  "Remove from pool: ..." summary, the yes/no prompt, and the go/no-go safety
  warnings attached to it (the "leaves 1 disk -- no RAID1 redundancy" line).
  Interactive-only by design; absent from `--dry-run` and `--yes` runs.
- **Classifier:** a *discovered precondition* is a note; a *consequence of the
  action the operator explicitly requested, surfaced to gate their go/no-go
  decision* is confirmation UI. The go/no-go warning is interactive-only in
  both commands, but what dry-run shows differs -- do not flatten the two:
  - `remove` 2->1 is a real redundancy *change*; dry-run surfaces it
    mechanically as the `RAID1 -> single` balance step (`remove.rs:188-197`),
    so the preview is never silent about the loss -- the warning only adds the
    human go/no-go prompt on top.
  - `replace`'s 1-disk warning fires on `pool.total_devices == 1`
    (`replace.rs:471`): the pool is non-redundant before and after, so there is
    no redundancy-changing step. Dry-run previews the replacement work
    (`btrfs replace start` ...); the warning is confirmation-only *context*
    that the remaining pool stays non-redundant. (The only redundancy-related
    step in replace, `restore raid1,soft` at `replace.rs:372-382`, *restores*
    redundancy on missing-path replacements and is unrelated to this warning.)

## Changes

### A. Mirror `replace`'s convention comments onto `remove.rs`

1. **`RemovePlan` struct doc** (`remove.rs:102-108`): append one sentence
   mirroring `replace.rs:179-180`, e.g.:
   > The 1-disk `WARNING:` (no RAID1 redundancy) is confirmation-UI, not a
   > `PreviewNote`: it stays behind the `!params.yes` gate and never appears
   > in `--dry-run` or on `--yes` runs.

2. **`preview()` doc** (`remove.rs:227`, currently undocumented): add a doc
   comment mirroring `replace.rs:395-397`, e.g.:
   > Build a `Preview` carrying any plan-derived notes. The 1-disk `WARNING:`
   > line stays in `execute()` behind the `!params.yes` gate and does not
   > appear here.

3. **Warning site** (`remove.rs:267-268`): add a 1-2 line comment at the
   `eprintln!`, pointing back to the struct/preview docs and naming the reason
   (go/no-go gate; consequence already shown as the `RAID1 -> single` step).
   `replace` has no site comment, but `remove`'s site is precisely where this
   finding originated, so the inline note is the highest-leverage spot.

### B. Add the missing regression test to `remove.rs`

Mirror `plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render`
(`replace.rs:4851-4910`). Critical design constraint: the test must drive the
**full dry-run path** `plan_remove(&runner, &fs, &params).preview().render()`
for a 2->1 removal -- **not** `RemoveWorkPlan::render_steps()` directly. The
regression being guarded is the finding's own proposed patch (pushing
`PreviewNote::Warn` in `plan_remove` when `remaining == 1`); such a note
surfaces only through `preview()`, so a `render_steps()`-only test (like the
existing `dry_run_render_2disk_removal_includes_balance`, `remove.rs:1299`)
would not catch it.

Assertions (load-bearing first, mirroring `replace.rs:4881-4892`):
- **Byte-equivalence (primary guard):** bind
  `let legacy = Step::render_dry_run(&preview.steps);` and assert
  `assert_eq!(rendered, legacy)` where `rendered = plan.preview().render()`.
  On a clean fixture the plan carries zero notes, so the full preview render
  must equal the steps-only render. *Any* note pushed in `plan_remove` for
  `remaining == 1` -- the current warning or a reworded variant like
  `"pool will have one disk..."` -- renders above the step block and breaks
  equivalence. Plain `!contains("WARNING:")` / `!contains("no RAID1 redundancy")`
  miss reworded notes, so byte-equivalence is the guard; keep one substring
  check only as a human-readable secondary assertion.
- `rendered.contains("RAID1 -> single")` -- the redundancy-loss consequence IS
  still surfaced mechanically as a step (the `remove`-specific behavior from
  the classifier; do not assume the analogue holds for `replace`).

This requires the 2-disk fixture to be clean enough that `plan_remove`
accumulates zero notes (rw mounted pool, no busy op, `check_single_survivor`
returns `EvictionCheck::Proceed` -- the same conditions under which `replace`'s
clean fixture yields zero notes). Mirror the dependency comment at
`replace.rs:4884-4888`.

Fixtures (already present, imported at `remove.rs:861-863`): build a 2-disk
healthy pool via `PoolFixture` / `RemovalPool`, with `valid_two_disk_usage_stdout`
+ `valid_two_disk_df_json` mocked so the single-survivor eviction check
(`check_single_survivor`) passes and `plan_remove` returns `Ok`. Follow the
`cmd_remove ... dry_run(true)` test setup already in the file.

Test name + preamble per Test Conventions (Intent / Why it exists / Scenario),
e.g. `plan_remove_2to1_preview_omits_confirmation_only_redundancy_warning`.

### C. Write the convention into Decision 022

Add a short labeled subsection to the **"Output contract"** section of
`docs/design/decisions/022-dry-run-preview-model.md` (headings in order:
Context, Decision, Output contract, Scope, Consequences, See). State the
classifier from "The convention being documented" above: confirmation-UI text
(the `!params.yes` summary/prompt and its go/no-go safety warnings, e.g.
`remove`/`replace`'s "leaves 1 disk -- no RAID1 redundancy") is deliberately
not a `PreviewNote` and is absent from `--dry-run`/`--yes`. Name `remove.rs`
and `replace.rs` as the two carriers so the scope is unambiguous.

Do **not** write a blanket "the consequence still renders as a Step" for both
commands -- that holds only for `remove` 2->1 (the `RAID1 -> single` balance).
Phrase it per command: `remove` 2->1 surfaces the redundancy-loss consequence
as a step, while `replace`'s 1-disk warning is confirmation-only context for a
pool that is non-redundant before and after, where dry-run previews the
replacement steps (no redundancy-changing step exists).

## Files to modify

- `cli/src/remove.rs` -- two doc comments + one site comment (A), one new test (B).
- `docs/design/decisions/022-dry-run-preview-model.md` -- Output-contract note (C).

No production logic changes. `replace.rs`, `add.rs`, `remove_missing.rs` are
unchanged (they already match the convention); they are only cited.

## Verification

- `just test-rust` -- new test passes; `replace`'s three guard tests and
  `add`/`remove_missing`'s note tests still pass (no behavior changed). Optional
  narrowing: `cargo test -p braid-cli plan_remove_2to1_preview` and
  `cargo test -p braid-cli plan_replace_live_preview`.
- Sanity-check the test actually guards the convention: temporarily add
  `notes.push(PreviewNote::Warn(...))` for `remaining == 1` in `plan_remove`
  with *any* wording (to confirm the byte-equivalence guard trips, not just a
  fixed substring), confirm the new test FAILS, then revert.
- `mdbook build docs` -- confirms the Decision 022 edit keeps the docs tree /
  linkcheck green (no new cross-links added, but cheap insurance).
- No VM tests needed: pure Rust + design-doc change, no systemd/lifecycle/mount
  blast radius.
