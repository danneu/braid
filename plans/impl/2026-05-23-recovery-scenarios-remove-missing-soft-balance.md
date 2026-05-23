# Plan: close the soft-balance gap in recovery-scenarios.md (Option B)

## Context

A code-review finding asked for a new guide titled "Recovering from a
Missing Disk in a RAID1 Pool" with sample `braid status` output and a
worked end-to-end walkthrough.

The verify-issue pass found the finding overstates the gap:
`docs/guides/recovery-scenarios.md` already exists and already has a
"Missing disk (drive failure)" section covering all four cited commands
(`unlock --allow-degraded`, `remove-missing --missing-id`, `replace`,
`status`). A new guide would duplicate it.

There is one real gap, though: the "Option B: Remove the missing device"
subsection (`recovery-scenarios.md:276-284`) does not mention that
`remove-missing` runs and blocks on a soft RAID1 balance when it clears
the last missing device with >= 2 disks remaining. That behavior is
documented in `docs/commands/remove-missing.md:66` and
`docs/design/principles.md:21`, but a reader following the recovery
walkthrough wouldn't know the command will sit and wait emitting
`[wait] pool: restoring RAID1 redundancy...` before returning.

The "Choosing" table at `recovery-scenarios.md:287-294` says
remove-missing takes "Minutes", which is fine for the primitive remove
but is misleading when there is meaningful single-profile data to
rebalance.

The intended outcome is a minimal, in-place patch to the existing
"Option B" subsection that closes the gap without duplicating the
command doc or expanding into a full sample-output walkthrough (which
would add maintenance burden every time `braid status` rendering
changes).

## Scope

In scope:

- Edit `docs/guides/recovery-scenarios.md` "Option B" subsection.
- Edit one cell in the "Choosing between replace and remove-missing"
  table in the same file (the `Restores redundancy` row,
  `remove-missing` column).

Out of scope:

- Creating a new guide file. The finding's proposed title would
  duplicate the existing "Missing disk (drive failure)" section.
- Adding literal `braid status` output blocks. They go stale on every
  status-rendering change and are not the established idiom in this
  file (recover's Verify step is just the command, no captured output).
- Editing `docs/guides/troubleshooting.md`. It is the symptom-first
  quick-fix index; the soft-balance detail belongs in the deep
  walkthrough doc, not the quick-fix one.
- Editing `docs/commands/remove-missing.md`. It already documents the
  soft balance step (line 66) and the sleep inhibitor (line 69).

## Critical file

`docs/guides/recovery-scenarios.md`:

- "Option B: Remove the missing device" subsection at lines 276-284.
- "Choosing between replace and remove-missing" table at lines
  287-294 (one cell edit; see Change 3 below).

## The change

Add two short additions to the existing "Option B" subsection. Mirror
the verify idiom already used by the recover walkthrough at
`recovery-scenarios.md:112-116` (just the command, no captured output).

1. **Soft-balance note in prose.** After the existing
   "Use this when you do not have a replacement disk..." paragraph,
   add one short paragraph along these lines (final wording at edit
   time):

   > When this clears the last missing device and 2+ disks remain,
   > `remove-missing` blocks on a follow-up soft RAID1 balance to
   > restore redundancy on chunks written as `single` during degraded
   > operation. You will see `[wait] pool: restoring RAID1
   > redundancy...` then `[ok]   pool: RAID1 redundancy restored`
   > before the command returns. The wait scales with how much data
   > was written while degraded; an idle pool finishes in seconds, a
   > pool written to heavily during degraded mode can take longer. A
   > sleep inhibitor is held for the entire operation. See
   > [`braid remove-missing`](../commands/remove-missing.md) for the
   > full sequence.

   Note on spacing: the status-line padding is byte-pinned in
   `cli/src/status_tag.rs:51-56` (`status_tag_pad`): `[ok]` takes 3
   trailing spaces, `[wait]` takes 1, to make a 7-column visible
   prefix. Use that exact spacing when quoting the lines (`[ok]   `
   = three spaces, `[wait] ` = one space).

2. **Verify step.** Add a "Verify:" block at the end of the
   subsection, identical in shape to
   `recovery-scenarios.md:112-116`:

   ```sh
   sudo braid status
   ```

   With one short trailing sentence noting what a successful result
   looks like: no missing devices, no `single` profile rows for data
   or metadata.

3. **Choosing-between table fix.** The "Choosing between replace and
   remove-missing" table at `recovery-scenarios.md:287-294` currently
   says `Restores redundancy | Yes | No`. That `No` contradicts the
   soft RAID1 balance step the new prose just described and is wrong
   on its own merits (`commands/remove-missing.md:66`,
   `design/principles.md:21`). Update the `remove-missing` cell of
   the `Restores redundancy` row to:

   > Partial: restores RAID1 profiles when 2+ disks remain, but does
   > not add replacement capacity

   That captures both halves of the truth: the soft balance restores
   the RAID1 profile on degraded-mode writes (the part the old `No`
   denied), and the pool still operates with one fewer disk so the
   missing device's share of capacity is gone (the part `Yes` would
   overstate).

Keep the rest of the table -- including the `Duration` row -- unchanged.
"Minutes" is still a fair single-cell approximation; the new prose
already sets expectations for the degraded-write case, which is where
the duration would skew. Splitting the duration cell into "minutes to
hours" risks scaring readers off a flow that is fast in the common
case.

## Reused existing patterns and references

- Verify idiom: `docs/guides/recovery-scenarios.md:112-116`
  (`Verify:` followed by a fenced `sudo braid status` block, no
  captured output).
- Authoritative source for the soft-balance behavior:
  `docs/commands/remove-missing.md:66-69` (step 7 + sleep inhibitor)
  and `docs/design/principles.md:21` (principle that both
  `remove-missing` and `replace` missing path run follow-up soft
  balance).
- Wait/ok phrase grounded in `cli/src/remove_missing.rs` and
  `cli/src/pool.rs` `maybe_restore_raid1` -- the exact stderr lines a
  reader will see (`[wait] pool: restoring RAID1 redundancy...` and
  `[ok]   pool: RAID1 redundancy restored`).
- Status-line padding rule: `cli/src/status_tag.rs:51-56`
  (`status_tag_pad`) pins `[ok]` to 3 trailing spaces and `[wait]` to
  1, byte-pinned by the test at
  `cli/src/status_tag.rs:212-235`
  (`status_line_prefix_is_seven_visible_columns`). The plan's quoted
  lines respect that exact spacing.

## Verification

This is a docs-only change with no runtime impact.

1. `mdbook build docs` -- mdbook-linkcheck (configured per the
   AGENTS.md doc rules and Decision 5) must pass. The new
   intra-`docs/` link to `../commands/remove-missing.md` is validated
   by linkcheck.
2. Read the modified "Option B" subsection top-to-bottom and confirm
   the prose flows: command -> caveat -> soft-balance note -> verify.
3. Re-read `docs/commands/remove-missing.md` and confirm the new
   guide prose does not contradict its "under the hood" list -- in
   particular the trigger condition ("last missing device and 2+
   disks remain") must match step 7 there.
4. Confirm the edited `Restores redundancy / remove-missing` cell
   reads exactly: `Partial: restores RAID1 profiles when 2+ disks
   remain, but does not add replacement capacity`. Confirm the
   `Duration` row and every other cell in the table is unchanged.
5. Confirm the quoted status lines in the soft-balance prose use the
   exact 7-column padding: `[wait] ` (one space) and `[ok]   ` (three
   spaces).

No code changes, no tests, no fixtures touched.
