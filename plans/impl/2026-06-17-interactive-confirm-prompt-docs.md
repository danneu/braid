# Plan: scope the "confirmation prompt" step to interactive runs in command docs

## Context

The end-user command pages under `docs/commands/` each have a "What happens under
the hood" section listing numbered steps. Three of those steps describe a
confirmation prompt that shows the target disk's identity (name/devid/by-id and
model/size/serial), but none of them note that this prompt is **interactive-only**.

That summary is built inside `execute()` behind the `if !params.yes` gate
(`cli/src/remove.rs#RemovePlan::execute`), and `--dry-run` returns from
`cli/src/remove.rs#cmd_remove` after `plan.preview().print_colored()` -- before
`execute` ever runs. So the dry-run preview renders only the plan notes plus the
btrfs/cryptsetup steps (`cli/src/remove.rs#RemovePlan::preview`); the disk-identity
summary never appears under `--dry-run` or `--yes`.

This is **correct, intended behavior**, codified in
[ADR 022 #confirmation-ui](../../docs/design/decisions/022-dry-run-preview-model.md):
"the interactive `!params.yes` block ... is deliberately absent from both
`--dry-run` and `--yes` output." The docs simply lag that convention. An operator
who reads "What happens under the hood", sees step N advertise the disk-identity
summary, and runs `--dry-run` to "see what would happen" will not see it -- an
expectation mismatch the docs invite but never resolve.

The same unscoped step recurs verbatim in three pages (a code-review finding flagged
only `remove.md`; the consistent fix covers all three). Outcome: each step states
the prompt is interactive-only, so the dry-run/confirm boundary is explicit and
matches the already-shipped behavior.

## Scope

- **Docs prose only.** No Rust, no ADR, no test changes.
- Three files, one appended sentence each. Existing step text is untouched.
- No `docs/guides/` or `docs/SUMMARY.md` changes -- neither duplicates these step
  lists.
- No `README.md` change, but for a specific reason (not because README omits the
  prompt). README's [Safety](../../README.md#safety) section *does* show this confirm
  prompt: its `### Confirm before it runs` subsection says "Without `--dry-run`, the
  data-shape commands (`add`, `remove`, `remove-missing`, `replace`) ... wait for you
  to type `yes`" and prints a sample `Remove from pool:` summary (name, hw line,
  `devid 3 | data will migrate ...`, `Pool: 3 disks -> 2 disks`). It is already
  correctly scoped -- the dry-run/confirm boundary is stated up front as "Without
  `--dry-run`" -- so it needs no edit. It is, in fact, the wording model the command
  pages are catching up to.

## The change

Append one consistent sentence to each confirmation-prompt step, leaving the
existing identity-payload wording intact:

> This interactive prompt is skipped by `--yes` and not shown in `--dry-run`.

Per-file (pure additions to the end of the existing step):

- **`docs/commands/remove.md`** -- step 3 (`Shows a confirmation prompt with the
  disk's name and devid, ... e.g. `Pool: 3 disks -> 2 disks`)`): append the sentence.
- **`docs/commands/add.md`** -- step 2 (`Shows a confirmation prompt with the disk's
  name and by-id path, ... omitted if unavailable)`): append the sentence.
- **`docs/commands/remove-missing.md`** -- step 4 (`Shows a confirmation prompt with
  the disk name, devid, ... e.g. `Pool: 2 present + 1 missing -> 2 disks`)`): append
  the sentence.

Rationale for this shape: an appended sentence (vs. a lead-in clause) keeps the
existing description of *what the prompt contains* unchanged and the verb-first list
style intact, makes the edit a minimal additive diff, and reuses the exact flag
names already in each page's "Important flags" table (`--yes` = "Skip interactive
confirmation", `--dry-run` = "Show what would happen without executing"). ASCII-clean
(backticks + `--`), and it renames no headings, so `mdbook-linkcheck2` is unaffected.

## Alternatives considered (rejected)

1. **Change the code so `--dry-run` prints the disk-identity summary.** Rejected:
   it contradicts ADR 022 #confirmation-ui, which deliberately keeps confirmation UI
   out of dry-run/`--yes`. The docs were wrong, not the code.
2. **Fix only `remove.md`** (as the finding proposed). Rejected: `add.md` and
   `remove-missing.md` carry the identical unscoped step and all three advertise
   `--dry-run`; fixing one leaves the inconsistency.
3. **Add a shared "dry-run vs confirmation" explainer page and link all command
   pages to it.** Rejected as over-engineering for a one-sentence clarification
   across three pages; it adds linkcheck surface for negligible DRY gain. The flag
   tables and "Common variations" already frame `--dry-run`/`--yes`.
4. **Also state what `--dry-run` shows instead** (the planned btrfs/cryptsetup
   steps). Left out to keep the step terse -- the `--dry-run` flag row and the
   "Preview what would happen" example already convey it. Easy to add later if a
   reviewer prefers it (e.g. append "; `--dry-run` previews the steps instead").

## Testing / verification

No new automated test -- this is a prose-only change with no behavior change. A
prose-wording assertion would be a brittle, structure-sensitive test (anti-pattern).

What makes the new sentence accurate for all three pages is the shared command
structure, not any one test: each entry point gates `--dry-run` to print
`plan.preview()` and `return` *before* the `!params.yes` confirm block in `execute()`
-- `cli/src/add.rs#cmd_add`, `cli/src/remove.rs#cmd_remove`, and
`cli/src/remove_missing.rs#cmd_remove_missing` -- which is exactly the boundary
ADR 022 #confirmation-ui mandates. So the disk-identity confirm prompt is
structurally unreachable under `--dry-run` in every case. Each command additionally
carries its own confirm-prompt and dry-run tests (`add_*_confirm_*` /
`cmd_remove_*_confirm_*` / `cmd_remove_missing_*_confirm_*`, plus `dry_run_*` render
tests), and `remove` has an explicit dry-run byte-pinning test (commit `eb27f23d`).
These already guard the behavior per-command; this change only makes the prose match
it.

Verify end-to-end:

1. `just check-docs` -- SUMMARY/table parity; passes (filenames, H1s, frontmatter
   all unchanged).
2. `just docs-build` -- runs `mdbook-linkcheck2`; passes (no new or renamed
   links/headings).
3. Eyeball the three rendered pages: the appended sentence reads correctly, the
   original step text is intact, and ASCII-only.
4. Sanity-check the behavior the docs now describe (optional, already covered by
   tests): `braid remove <name> --dry-run` prints the step preview with **no**
   name/devid/model summary; a real `braid remove <name>` (no `--yes`) shows the
   summary in the confirm prompt.
