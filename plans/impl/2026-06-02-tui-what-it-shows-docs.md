# Fix stale "What it shows" section in docs/commands/tui.md

## Context

A High/Accuracy finding flagged that the TUI doc's disk-table description
(`docs/commands/tui.md:60`) lists columns that don't exist ("size",
"unallocated") and omits shipped ones (the row number and the "Temp" column).
Investigation confirmed the finding **and** found it is one of **four** stale
claims in the same "What it shows" section, all from one root cause: the doc
drifted as TUI features were added (per-disk temperature column, the popup's
allocations table, three more Browse programs) without the doc being updated.
The keybindings table is the only part of the section still accurate.

This is a docs-only change. No code is wrong; the doc must be brought back in
sync with what the TUI renders. README.md does not duplicate this content
(verified), so there is no second doc to sync.

## Scope decision

User chose the **whole-section** accuracy pass (not just the cited line).

## The four edits (all in `docs/commands/tui.md`)

Each rewrite below is grounded in code read during investigation. Cited code
lines are for the implementer to re-verify, not to modify.

### 1. Main view -- line 52-58 (drop phantom "scrub state")

Code: `pool_info` at `cli/src/tui/view/mod.rs:400-481` renders only Path,
Profile, Balance (when not idle), and Usage; alerts **and** advisories render
at the top (`view`, ~`1446-1498`). Scrub state is **not** here -- it lives on
the Scrub tab (the doc itself says so at line 67), so listing it in the main
view is both wrong and self-contradictory.

Change the element list from
`...capacity bar, scrub state, balance state, and active alerts.`
to
`...capacity bar, balance state, and active alerts and advisories.`
(Drops "scrub state"; broadens "active alerts" to "active alerts and
advisories" to match what renders. Leave the `Profile` summary parenthetical
untouched -- it is accurate.)

### 2. Disk table -- line 60 (the original finding)

Code: header `Row::new(["", "Name", "Bus", "SMART", "Temp", "btrfs",
"Allocated"])` at `cli/src/tui/view/mod.rs:849`; the Allocated cell renders
`NN%  alloc / size unit` at `:889-895`; helpers `temperature_cell` (`:784`),
`btrfs_cell` (`:803`, renders `N err`).

Replace line 60 with the finding's verified wording:
`**Disk table** -- one row per disk: number, name, bus (sata/usb/nvme), SMART
health, temperature, btrfs device-error count, and allocated (shown as percent
used and allocated/size).`

### 3. Disk detail popup -- line 62 (remove SMART, add real fields)

Code: `view_disk_detail` at `cli/src/tui/view/mod.rs:1216-1354`. Actual fields:
Disk name (`:1239`), Status / lock state unlocked|locked|unknown (`:1243`),
Cipher / Key size / Keyslots (`:1256-1268`), an **Allocations table**
(Type/Profile/Size + an Unallocated row, `:1282-1326`), and the btrfs Device
Errors breakdown (read/write/flush/corruption/generation, `:1328-1354`). There
is **no SMART health** rendered anywhere in the popup -- the current doc claim
is false.

Replace line 62 with:
`**Disk detail popup** (press Enter on a disk) -- disk name, LUKS lock status,
cipher, key size, keyslot count, an allocations table (type/profile/size plus
unallocated), and the btrfs device-error breakdown
(read/write/flush/corruption/generation).`
(Removes the false "SMART health"; adds disk name, lock status, and the
allocations table. The conditional "underlying device gone" warning is a
transient state line, not a standing field -- intentionally omitted.)

### 4. Browse -- line 68 (2 program families -> 5)

Code: `BrowseProgram` enum at `cli/src/tui/browse/state.rs:25-50` has five
families with labels Btrfs, NUT, Systemd, SMART, lsblk. Command groups
(`BrowseCommand`, `:55-86`): Systemd = status/show/braid/failed/timers/mounts;
Smartctl(SMART) = scan/health/info/attributes/self-test log/error log; lsblk =
tree/filesystems/disks/all-columns/SCSI. The Btrfs and UPS/NUT sentences
already present are accurate and stay.

Rewrite the Browse bullet's opening so it names all five families, then keep the
existing Btrfs and UPS sentences and add one concise sentence each for the three
missing families. Opening becomes, e.g.:
`**Browse** -- raw CLI output inspector across five tool families: Btrfs, NUT
(UPS), Systemd, SMART (smartctl), and lsblk.`
Then, after the existing Btrfs and UPS sentences, add:
`Systemd views include unit status, show, braid units, failed units, timers,
and mounts. SMART views include device scan, health, info, attributes, and
self-test/error logs. lsblk views include tree, filesystems, disks, all-columns,
and SCSI.`
Keep the existing `NUT > UPSes` helper note.

## Not changed (verified accurate -- do not touch)

- **Keybindings table (lines 37-48):** matches `cli/src/tui/keymap.rs` and
  `cli/src/tui/browse/keymap.rs` exactly. No change.
- **"When to use it" line 10** ("LUKS cipher, SMART health, error counts,
  transport"): describes disk-level detail available in the TUI generally;
  SMART health *is* shown (disk-table column), so this stays accurate. No change.
- **Tab list lines 64-67** (Data / Scrub bullets): accurate. Only the Browse
  bullet (line 68) changes.

## Files

- `docs/commands/tui.md` -- only file modified (4 edits above).

## Verification

Use repo-local invocations -- neither `mdbook` nor the installed `braid` is on
PATH in this repo's shell, so ambient commands would be flaky or verify a stale
binary.

1. **Build the book (repo-local):** `nix develop .#docs -c mdbook build docs` --
   mdbook lives only in the `docs` devShell (`flake.nix:1074`; `just docs` uses
   the same shell). Confirms the page builds and `mdbook-linkcheck2` passes
   (edits are prose-only and add no cross-links, so this is a low-risk smoke
   check). `just check-docs` (`justfile:215`) is the canonical one-shot
   SUMMARY-parity + link-integrity check if you want belt-and-suspenders.
2. **Ground-truth against the live TUI (repo-local):**
   `cargo run --manifest-path cli/Cargo.toml --bin braid -- tui --demo` -- the
   installed `braid` is not on PATH, so run from source (bin `braid`, package
   `braid-cli`). Demo mode needs no root, config, or btrfs (see tui.md:19-27).
   Visually confirm each rewritten claim against the running widgets:
   - Disk table header reads `Name Bus SMART Temp btrfs Allocated` with a leading
     number column.
   - Press Enter on a disk: popup shows Disk/Status/Cipher/Key size/Keyslots, an
     Allocations table, and the btrfs Device Errors table -- and **no** SMART row.
   - Main view shows Path/Profile/Balance/Usage (no scrub line).
   - Tab to Browse: the program column lists Btrfs, NUT, Systemd, SMART, lsblk.
   (Demo data may not populate every value, e.g. temperature/SMART, but column
   headers, popup field labels, and the Browse program list all render.)
3. **Spot-check the cited code** for each rewritten claim if any wording is
   uncertain: `view/mod.rs:849` (table header), `:889-895` (allocated cell),
   `:1216-1354` (popup), `:400-481` (main view), `browse/state.rs:25-86`
   (Browse families and commands).

## Note on recurrence

These prose docs have no automated code-sync guard, which is why they drifted.
Building doc-codegen or a header-assertion test for one mdBook page is not worth
it; instead, the demo-mode walkthrough in step 2 (`cargo run ... -- tui --demo`)
should be the standing reviewer check whenever TUI widgets change.
