# Fix: TUI disk-detail popup doc claims SMART health it does not render

## Context

`docs/commands/tui.md:62` describes the disk-detail popup (press Enter on a
disk in the Data tab) as showing "LUKS cipher, key size, keyslot count, device
errors breakdown ... and SMART health." Two things are wrong:

1. **The popup renders no SMART health.** `view_disk_detail`
   (`cli/src/tui/view/mod.rs:1216-1441`) builds only: Disk name, lock `Status`,
   Cipher, Key size, Keyslots, an `Allocations` table, and a `btrfs Device
   Errors` table. `smart_cell` (`mod.rs:770`) is called only from the Data-tab
   disk *table* (`mod.rs:847-918`), never the popup. A reader who opens the
   popup expecting SMART will not find it.
2. **The doc omits the popup's two distinguishing elements** -- the LUKS lock
   `Status` line and the per-disk `Allocations` table -- which are the actual
   reason to open the popup instead of reading the table row.

SMART health is already correctly documented one line up, at `tui.md:60`, as a
column of the disk table. So the fix removes a misplaced-and-duplicated claim
and adds the two missing elements; no information is lost.

The wrong claim has been present since the docs tree was unified (`403d1b07`)
and was never corrected. This is unreleased software with no doc-accuracy test,
so the drift went unnoticed.

## Change

Single-line rewrite of `docs/commands/tui.md:62`.

**Before:**

```
**Disk detail popup** (press Enter on a disk) -- LUKS cipher, key size, keyslot count, device errors breakdown (read/write/flush/corruption/generation), and SMART health.
```

**After:**

```
**Disk detail popup** (press Enter on a disk) -- LUKS lock status, cipher, key size, keyslot count, a per-disk allocation table (data/metadata/system/unallocated), and the btrfs device-errors breakdown (read/write/flush/corruption/generation).
```

Why this wording is correct against the code:

- **LUKS lock status** -- the always-present `Status` line, values
  `unlocked`/`locked`/`unknown` (`mod.rs:1223-1246`).
- **cipher, key size, keyslot count** -- rendered when LUKS metadata is present
  (`mod.rs:1256-1268`); unchanged from the current text.
- **per-disk allocation table (data/metadata/system/unallocated)** -- the
  `Allocations` table (`mod.rs:1282-1326`). Row types come from `btrfs device
  usage` as `Data`/`Metadata`/`System` (`parse/btrfs_device_usage.rs`), plus a
  synthesized `Unallocated` row (`mod.rs:1298-1309`).
- **btrfs device-errors breakdown (read/write/flush/corruption/generation)** --
  the `btrfs Device Errors` table; labels match exactly (`mod.rs:1331-1337`).
  Lowercase in the doc matches the rendered lowercase labels.

Ordering matches the popup's top-to-bottom render order (the `Disk` name header
is omitted as trivially "the disk you selected"). The conditional
`underlying device gone` / `LUKS metadata unavailable` fallback lines are
intentionally left undocumented -- they are edge states, not the steady-state
contract, and listing them would bloat a terse reference page.

## Deliberate non-changes

- **`tui.md:60`** (disk table lists `... SMART health, and error counts`) --
  correct; SMART genuinely lives here. Leave as-is. This is also why the fix
  *drops* rather than *relocates* the SMART claim.
- **`tui.md:10`** ("Checking disk-level detail (LUKS cipher, SMART health,
  error counts, transport)") -- this is a "When to use it" use-case bullet
  describing disk inspection across the whole TUI (popup + table), not a
  per-widget contract. SMART health is inspectable in the disk UI, so the
  bullet is accurate. Leave as-is to keep the fix focused; revisit only if the
  user wants the page rewritten to a strict per-widget framing.

## Files to modify

- `docs/commands/tui.md` (line 62 only).

No code changes. No other doc touches required.

## Verification

1. **Accuracy (primary):** `cargo run -p braid-cli -- tui --demo` (no root, no
   pool needed). On the default Data tab, select a disk with `j`/`k` and press
   `Enter`. Confirm the popup shows: a `Status` line, `Cipher`/`Key size`/
   `Keyslots`, an `Allocations` table, and a `btrfs Device Errors` table -- and
   **no** SMART field. Demo data populates allocations and device errors for
   all three sample disks (`cli/src/tui/demo.rs:43-116,159+`), so all popup
   sections render. Confirm the new line 62 enumerates exactly these and
   nothing absent.
2. **Docs build / link integrity:** `mdbook build docs` succeeds. The edit
   changes prose only (no links), so `mdbook-linkcheck2` should pass
   unchanged; this is a guard against accidental Markdown breakage.

## Out of scope

No new tests. The repo has no prose-accuracy test harness for docs, and adding
one for a single line is disproportionate; the demo-mode visual check above is
the appropriate end-to-end confirmation.
