# Plan: collapse duplicate "Basic example" blocks in `docs/commands/replace.md`

## Context

The "Basic example" section of `docs/commands/replace.md` (lines 12-24) presents
two fenced command blocks -- one labeled "Replace a live disk", one labeled
"Replace a dead/missing disk" -- that are **byte-identical**:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1
```

This is *correct* (the argv genuinely is the same for both cases), but presenting
two identical blocks reads like a copy-paste mistake and gives the operator no
signal about what differs. It also reinforces an inaccurate mental model that
the dead-disk case needs a different command or `--missing-id`.

**Verified against code** (`cli/src/replace.rs:1649` `resolve_replace_source`):
braid discriminates live vs missing purely from observed pool state. If `--old`'s
LUKS UUID is present among live `pool.devices` it's a live replace; if absent it
falls back to the persisted devid in `pool.json` (the dead/missing path). The
operator never selects a mode. `--missing-id` is **rejected** for live disks
(lines 1661-1665) and is only an optional cross-check on the missing path
(auto-resolve at 1733-1761) -- consistent with the flag table's "Never required"
(line 75) and the arg help in `cli/src/main.rs:335`.

This premise is regression-covered by existing unit tests in `cli/src/replace.rs`:
live resolution (~2222), missing auto-resolution (~2481), multiple-missing
persisted-devid selection (~2541), and `--missing-id` mismatch rejection (~2727).
No new tests are needed -- the edit is prose-only.

**Intended outcome:** one example block plus a one-line note that explains *why*
one command covers both cases, dissolving both the copy-paste appearance and the
`--missing-id` confusion.

## The change

**File:** `docs/commands/replace.md` only.

Replace the current "Basic example" body (lines 14-24, i.e. the two labeled
blocks) with a single note + single block. Recommended text:

```markdown
## Basic example

The same invocation replaces a disk whether it is still live or already
dead/missing. braid resolves `--old` against `pool.json` to find the member and
detects its state automatically, so there is no mode to choose and `--missing-id`
is never required:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1
```
```

## Scope boundaries (do NOT touch)

- **Leave line 5 as-is** -- "Works for both live (still-online) and dead/missing
  disks" already frames both cases and complements the new note.
- **Leave "Common variations" as-is** -- the genuine `--missing-id` example
  (lines 36-43) and the hot-unplug note (lines 28-34) already stand on their own;
  the collapsed block does not duplicate them.
- **No README change.** `README.md` already shows `replace` with single examples
  (lines 26-27, 110-111), so the README/docs sync obligation in `AGENTS.md` is
  already satisfied. Confirmed no other `docs/commands/*.md` repeats this
  duplicate-block pattern, so there is no sibling fix to make.

## Working-tree state

`docs/commands/replace.md` has **no current staged or unstaged changes**. The
exclusive-op `--enqueue` edit that was briefly staged here has since landed in
commit `3d679db` and is now in HEAD at line 103 -- below the "Basic example"
section, so target lines 14-24 are unaffected. This edit lands on a clean file
and is its own commit. Other dirty files in the working tree are unrelated and
outside this plan's scope.

## Verification

This is a prose-only edit with no automated test coverage:

1. Confirm the duplicate is gone:
   `awk '/^```/{f=!f;next} f&&/braid/{if($0==p)print "DUP:"$0; p=$0}' docs/commands/replace.md`
   should print nothing.
2. `mdbook build docs` succeeds (no links touched, but confirms the page still
   renders and `mdbook-linkcheck` passes).
3. Visual read: "Basic example" now shows one block; "Common variations" still
   shows the distinct `--missing-id` example.
