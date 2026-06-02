# Fix "pool size" -> "disk count" in remove / remove-missing docs

## Context

The mdBook command reference describes the `braid remove` and
`braid remove-missing` confirmation prompts as showing "the resulting **pool
size**." That wording is inaccurate: neither prompt shows a capacity figure
(TiB). Both show a disk **count** transition. A reader primed to expect a
capacity number is confused when the real output is a count -- and the prose
contradicts `README.md`, which renders the actual line `Pool: 3 disks -> 2
disks`.

The render strings are owned and pinned by unit tests, so they are a
drift-proof source of truth to quote:

- `cli/src/remove.rs#format_remove_confirm` emits `Pool: {total} disks -> {remaining} disks`; test asserts `"3 disks -> 2 disks"` (`cli/src/remove.rs` tests).
- `cli/src/remove_missing.rs#format_remove_missing_confirm` emits `Pool: {present} present + {missing} missing -> {present} disks` (single-missing case); test asserts `"2 present + 1 missing -> 2 disks"`.

The removed disk's *own* size (e.g. `12.00 TiB`) **is** shown in the `remove`
prompt, via the shared `cli/src/confirm.rs#format_hw_info_line` (`model | size |
serial`) -- but `remove.md`'s field list omits it. `remove-missing` operates on
a missing device and shows **no** hardware/size info ("no hardware info
available").

Outcome: the two doc lines accurately describe what each prompt prints.

## Edits

Two doc lines, both pure prose. No code, no tests, no other files.

### 1. `docs/commands/remove.md:46`

Rename "pool size" -> "disk count", add the concrete example, and add the
omitted `size` field (shown in the prompt's hw line, between `model` and
`serial` to match render order).

- From:
  `3. Shows a confirmation prompt with the disk's name, model, serial, devid, and the resulting pool size`
- To:
  `` 3. Shows a confirmation prompt with the disk's name, model, size, serial, devid, and the resulting disk count (e.g. `Pool: 3 disks -> 2 disks`) ``

### 2. `docs/commands/remove-missing.md:63`

Same defect, but this command's render differs and it shows no hardware info --
so use **its own** example (present/missing form) and do **not** add a `size`
field. Note the **plural** "disk counts" here (vs. singular "disk count" in
`remove.md`): `format_remove_missing_confirm` has two branches, and the
multi-missing one (`missing_count >= 2`, hit when clearing 2+ dead disks one at
a time) decomposes both sides -- `Pool: N present + M missing -> N present +
(M-1) missing` (pinned by the `2 present + 2 missing -> 2 present + 1 missing`
test) -- not a single total. The plural honestly covers that branch too;
`remove.md` stays singular because both its branches (3->2, 2->1) are plain
single counts.

- From:
  `4. Shows a confirmation prompt with the disk name, devid, and resulting pool size`
- To:
  `` 4. Shows a confirmation prompt with the disk name, devid, and the resulting disk counts (e.g. `Pool: 2 present + 1 missing -> 2 disks`) ``

## Out of scope (verified, intentionally untouched)

- **`docs/commands/add.md:86`** -- already accurate ("model, serial, and size");
  `cli/src/add.rs#format_add_confirm` shows the disk's own size and no pool
  total. No "pool size" claim.
- **`docs/commands/replace.md`** -- no "pool size" / "confirmation prompt"
  language in its "What happens under the hood" section. Not affected.
- **`README.md`** -- already the ground-truth render (`Pool: 3 disks -> 2
  disks`). Correct as-is.
- **Code (`format_remove_confirm`, `format_remove_missing_confirm`)** -- correct
  and intentional (count format was deliberately standardized in commit
  `6a70d179 unify confirmation prompts across add, remove, remove-missing,
  replace`). This is a docs-only fix; do not change the prompts.
- `docs/commands/doctor.md` and `docs/design/decisions/001-btrfs-raid1.md` also
  match `rg "pool size"`, but both use the phrase for unrelated concepts
  (kernel chunk threshold; redundancy model). Leave them.

## Verification

1. `mdbook build docs` -- builds the book and runs `mdbook-linkcheck2`; confirms
   the edits don't break rendering or cross-links (the added backtick code spans
   are inline, not links).
2. `rg -n "pool size" docs/commands/remove.md docs/commands/remove-missing.md`
   returns nothing.
3. Eyeball the two rendered list items against the live prompt strings, which
   stay pinned by the existing formatter unit tests (`just test-rust` already
   covers them -- no new test needed and none changed).
