# Plan: `braid browse` — lightweight btrfs command browser TUI

## Context

Managing a btrfs NAS means constantly running `btrfs filesystem usage`, `btrfs device stats`, `btrfs subvolume list`, etc. The commands are hard to remember and tedious to type repeatedly. `braid browse` is a thin TUI wrapper that organizes read-only btrfs commands into tabs, dumps the raw output, and lets you press `r` to reload. It reuses the same Elm-style MVU boilerplate as the existing `braid tui`, and runs all commands through the existing `CmdRequest` + `CommandRunner` infrastructure.

## Tab / subtab structure

```
Filesystem   Devices   Subvolumes   Scrub   Balance
[Usage]  Show  Df
```

| Tab | Subtabs | Command |
|-----|---------|---------|
| Filesystem | Usage, Show, Df | `btrfs filesystem usage {mp}`, `btrfs filesystem show {mp}`, `btrfs filesystem df {mp}` |
| Devices | Usage, Stats | `btrfs device usage {mp}`, `btrfs device stats {mp}` |
| Subvolumes | *(none)* | `btrfs subvolume list {mp}` → j/k select → Enter → `btrfs subvolume show {mp}/{path}` |
| Scrub | Status | `btrfs scrub status {mp}` |
| Balance | Status | `btrfs balance status {mp}` |

## Key bindings

| Key | Action |
|-----|--------|
| Tab / Shift-Tab | next / prev primary tab |
| h/l, Left/Right | next / prev subtab |
| j/k, Down/Up | scroll output (or select subvol in Subvolumes tab) |
| Ctrl-D / Ctrl-U | page down / page up |
| Enter | drill into subvolume detail (Subvolumes tab only) |
| Esc / Backspace | back from detail view |
| r | reload current command |
| q / Ctrl-C | quit |
| ? | toggle help |

## New CmdRequest variants

`cli/src/cmd.rs` — add these variants for human-readable display output (the existing `*Raw`/`*Json` variants use machine-readable flags):

```rust
BtrfsFilesystemUsage { mount_point: MountPoint }    // without --raw
BtrfsFilesystemDf { mount_point: MountPoint }        // without --format json
BtrfsDeviceUsage { mount_point: MountPoint }         // without --raw
BtrfsSubvolumeList { mount_point: MountPoint }       // new command
BtrfsSubvolumeShow { path: String }                  // new command
```

Reuse existing variants as-is where they already produce human-readable output:
- `BtrfsFilesystemShow` — already fine
- `BtrfsDeviceStats` — already fine
- `BtrfsScrubStatus` — already fine
- `BtrfsBalanceStatus` — already fine

## New parser: `btrfs subvolume list`

`cli/src/parse/btrfs_subvolume_list.rs` — nom parser for structured subvolume data.

`btrfs subvolume list /mnt/storage` output format:
```
ID 256 gen 30 top level 5 path snapshots/daily
ID 257 gen 25 top level 5 path backups
ID 258 gen 42 top level 256 path snapshots/daily/2026-03-01
```

Each line: `ID <id> gen <gen> top level <top_level> path <path>`

### Output type (`cli/src/parse/types.rs`)

```rust
pub struct BtrfsSubvolume {
    pub id: u64,
    pub gen: u64,
    pub top_level: u64,
    pub path: String,
}

pub struct BtrfsSubvolumeListOutput {
    pub subvolumes: Vec<BtrfsSubvolume>,
}
```

### Parser structure

- `parse_subvolume_line(input: &str)` — nom parser for one line: `ID <u64> gen <u64> top level <u64> path <rest>`
- `parse_btrfs_subvolume_list(raw: &RawCommandOutput) -> Result<BtrfsSubvolumeListOutput, ParseError>` — iterates lines, parses each, collects into vec
- Empty output (no subvolumes) returns empty vec, not an error

### Tests

- **Fixture test**: `tests/fixtures/nixos-25.11/btrfs-subvolume-list.txt` — capture from a real system with a few subvolumes
- **Synthetic inline tests**: multi-subvolume, empty output, path with spaces, deeply nested path

### Registration

- `cli/src/parse/mod.rs` — add `pub mod btrfs_subvolume_list;` and re-export `parse_btrfs_subvolume_list`

## Browse TUI architecture

Module: `cli/src/browse/`

Reuses the same MVU pattern as `cli/src/tui/`: InputHandler thread → mpsc → event loop (16ms frame budget) → messages → update → effects → view.

### `mod.rs` — entry point + event loop + effect execution

- `pub fn run(mount_point: &str) -> io::Result<()>` — init terminal, create `InputHandler`, create `Model` (returns initial `RunCommand` effect), `run_loop`, restore terminal
- `run_loop()` — identical shape to existing TUI: 16ms frame budget, batch ≤100 events, apply messages, execute effects
- Effect execution: `Effect::RunCommand { request, generation }` spawns a thread, uses `RealRunner.run(&request)`, sends back `Event::CommandFinished { raw, generation }`
- Generation counter: `u64` in Model, incremented on each new command. `CommandFinished` with stale generation is ignored.

### `model.rs` — state

- `Tab` enum: Filesystem, Devices, Subvolumes, Scrub, Balance — with `ALL`, `label()`, `next()`, `prev()`, `subtabs()`
- `SubTab` enum: FsUsage, FsShow, FsDf, DevUsage, DevStats, SubvolList, ScrubStatus, BalanceStatus — with `label()` and `request(mount_point) -> CmdRequest`
- `ViewMode` enum: Normal, SubvolDetail, Help
- `Model` struct: running, mode, mount_point (as `MountPoint`), tab, subtab_index, output (`Vec<String>`), scroll_offset, loading, frame, command_gen, subvolumes (`Vec<BtrfsSubvolume>`), subvol_selected, viewport_height

### `app.rs` — messages + update

Messages: Quit, ToggleHelp, NextTab, PrevTab, NextSubTab, PrevSubTab, ScrollDown, ScrollUp, PageDown, PageUp, Select, Back, Reload, Tick, CommandFinished { raw: RawCommandOutput, generation: u64 }

Key update logic:
- Tab/subtab switch → reset scroll, set loading, return `Effect::RunCommand` with `current_subtab().request(mount_point)`
- `CommandFinished` → if generation matches: store `raw.stdout` lines in `model.output`. If current subtab is SubvolList and mode is Normal: also parse via `parse_btrfs_subvolume_list()` and store in `model.subvolumes`
- `Select` (Subvolumes tab) → switch to SubvolDetail, fire `RunCommand` for `BtrfsSubvolumeShow { path: "{mount_point}/{subvol.path}" }`
- `Back` → return to Normal mode, restore subvol list output

### `event.rs` — events + InputHandler

Copy the `InputHandler` pattern from `cli/src/tui/event.rs` (40-line crossterm polling thread), adapted to this module's `Event` type:
```rust
enum Event {
    Key(KeyEvent),
    CommandFinished { raw: RawCommandOutput, generation: u64 },
    Tick,
}
```

### `keymap.rs` — key → message mapping

`handle_key(key, mode) -> Option<Message>` — mode-aware dispatch as described in key bindings table.

### `view.rs` — rendering

Layout (top to bottom):
1. Tab bar (1 line) — active: cyan+bold+underlined, inactive: dark gray
2. Subtab bar (1 line, only if >1 subtab) — active: cyan+bold, inactive: dark gray
3. Body (fill) — raw command output, scrollable. In Subvolumes tab: `>` marker on selected row
4. Command line (1 line, dim) — `$ btrfs filesystem usage /mnt/storage` with spinner if loading
5. Footer (1 line, dark gray) — context-sensitive key hints

Help overlay: centered popup (same pattern as `cli/src/tui/view/help.rs`).

## Tests

Following the same patterns as `cli/src/tui/`:

### Parser tests (`cli/src/parse/btrfs_subvolume_list.rs`)

- **Fixture test**: parse `tests/fixtures/nixos-25.11/btrfs-subvolume-list.txt`, assert subvolume count and fields
- **Synthetic inline tests**: multi-subvolume, empty output (no subvols = empty vec), path with spaces, deeply nested path, non-zero exit code returns `CommandFailed`

### Update tests (`cli/src/browse/app.rs`)

- `next_tab_resets_scroll_and_loads` — tab switch resets scroll_offset to 0 and returns RunCommand effect
- `next_subtab_wraps_and_loads` — subtab cycles and fires command
- `stale_command_ignored` — CommandFinished with old generation doesn't update output
- `select_enters_subvol_detail` — in Subvolumes tab with subvolumes, Select switches to SubvolDetail and returns RunCommand for subvolume show
- `back_returns_to_normal` — from SubvolDetail, Back restores Normal mode
- `reload_when_loading_is_noop` — Reload while loading returns no effects

### Keymap tests (`cli/src/browse/keymap.rs`)

- `r_reloads_in_detail_mode` — pressing r in SubvolDetail emits Reload
- `esc_goes_back_in_detail` — Esc in SubvolDetail emits Back
- `tab_in_detail_is_ignored` — Tab key in SubvolDetail returns None (no tab switching from detail)
- `h_l_switch_subtabs` — h/l emit PrevSubTab/NextSubTab in Normal mode

### View snapshot tests (`cli/src/browse/view.rs`)

Using ratatui `TestBackend` + `insta::assert_snapshot!()`, same pattern as existing TUI:

- `snapshot_filesystem_usage` — Filesystem tab, Usage subtab, with sample output
- `snapshot_filesystem_show` — Filesystem tab, Show subtab selected
- `snapshot_devices_tab` — Devices tab with subtab bar
- `snapshot_subvolumes_with_selection` — Subvolumes tab, j/k selection highlight
- `snapshot_subvol_detail` — SubvolDetail mode showing detail output
- `snapshot_loading` — Loading spinner state
- `snapshot_help` — Help overlay
- `snapshot_single_subtab_no_bar` — Scrub tab (1 subtab), subtab bar hidden

Snapshot files go in `cli/src/browse/view/snapshots/` (or `cli/src/browse/snapshots/` if view.rs is a flat file).

### Test helpers

`Model::new_demo(mount_point, tab, output_lines)` constructor for tests — sets up model with given tab, dummy output lines, no loading state. Similar to existing `Model::new_demo` in the main TUI.

### NixOS VM integration test

`braid browse` is interactive, so we can't test it directly in a headless VM. Add a `--check` flag that exercises the full command pipeline non-interactively:

1. Runs `btrfs filesystem usage <mp>` (proves Filesystem tab's default command works)
2. Runs `btrfs subvolume list <mp>` and parses the output
3. If subvolumes exist, runs `btrfs subvolume show <mp>/<first_subvol_path>` (proves drill-in works)
4. Exits 0 if all succeed, non-zero with error details if any fail

**`tests/cli/braid-browse.nix`** — VM config:
- 2 virtual disks (256MB each), initrd-fixture formats LUKS + btrfs RAID1
- braid module enabled, braid package available
- Uses same pattern as `tests/module/raid1.nix`

**`tests/cli/braid-browse.py`** — Test script:
```python
start_all()
machine.wait_for_unit("multi-user.target")

with subtest("unlock pool"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("create subvolume for drill-in test"):
    machine.succeed("btrfs subvolume create /mnt/storage/test-subvol")

with subtest("braid browse --check exercises command pipeline"):
    output = machine.succeed("braid browse --check")
    assert "filesystem usage" in output.lower() or "ok" in output.lower()

machine.shutdown()
```

**`flake.nix`** — Register:
```nix
braid-browse = pkgs.testers.nixosTest (
  import ./tests/cli/braid-browse.nix { braid = linuxCrane.braid; }
);
```

## README update

Add a section for `braid browse` after the existing TUI dashboard mention (README.md, near the "Dashboard" feature bullet and after the "Pool status" section). Brief, cookbook-style:

```markdown
### Browse btrfs commands

Interactive read-only browser for raw btrfs command output:

    sudo braid browse

Tabs: Filesystem, Devices, Subvolumes, Scrub, Balance. Each tab runs the corresponding `btrfs` command and dumps the output. Press `r` to reload, `Tab`/`Shift-Tab` to switch tabs, `h`/`l` for subtabs, `j`/`k` to scroll. In the Subvolumes tab, `Enter` drills into a selected subvolume's detail.

    sudo braid browse --check    # non-interactive: verify all commands succeed
```

## Files to create

| File | Purpose |
|------|---------|
| `cli/src/browse/mod.rs` | Entry point, event loop, effect execution, `--check` mode |
| `cli/src/browse/model.rs` | Tab, SubTab, ViewMode, Model |
| `cli/src/browse/app.rs` | Message, update() + tests |
| `cli/src/browse/event.rs` | Event, InputHandler |
| `cli/src/browse/keymap.rs` | handle_key() + tests |
| `cli/src/browse/view.rs` | view(), tab_bar(), help overlay + snapshot tests |
| `cli/src/parse/btrfs_subvolume_list.rs` | nom parser + tests |
| `cli/tests/fixtures/nixos-25.11/btrfs-subvolume-list.txt` | Fixture for parser |
| `tests/cli/braid-browse.nix` | NixOS VM test config |
| `tests/cli/braid-browse.py` | NixOS VM test script |

## Files to modify

| File | Change |
|------|--------|
| `cli/src/cmd.rs` | Add 5 new CmdRequest variants + to_argv() arms |
| `cli/src/parse/mod.rs` | Add `pub mod btrfs_subvolume_list;` + re-export |
| `cli/src/parse/types.rs` | Add `BtrfsSubvolume`, `BtrfsSubvolumeListOutput` |
| `cli/src/lib.rs` | Add `pub mod browse;` |
| `cli/src/main.rs` | Add `Browse(BrowseArgs)` with `--check` flag + dispatch |
| `flake.nix` | Register `braid-browse` VM test |
| `README.md` | Add `braid browse` section |

## Implementation order

1. `cmd.rs` — add 5 new CmdRequest variants + to_argv()
2. `parse/types.rs` — add BtrfsSubvolume, BtrfsSubvolumeListOutput
3. `parse/btrfs_subvolume_list.rs` — nom parser + fixture + tests
4. `parse/mod.rs` — register module + re-export
5. `browse/model.rs` — Tab, SubTab, ViewMode, Model (including new_demo for tests)
6. `browse/app.rs` — Message, update() + tests
7. `browse/keymap.rs` — handle_key() + tests
8. `browse/event.rs` — Event, InputHandler
9. `browse/view.rs` — view() + snapshot tests
10. `browse/mod.rs` — run(), run_loop(), effect execution, check mode
11. `lib.rs` + `main.rs` — wiring (Browse command with `--check` flag)
12. `tests/cli/braid-browse.nix` + `braid-browse.py` — NixOS VM test
13. `flake.nix` — register VM test
14. `README.md` — document `braid browse`

## Verification

1. `cargo test -p braid-cli` — all tests pass (parser, update, keymap, snapshots)
2. `cargo build -p braid-cli` — compiles cleanly
3. `just test braid-browse` — NixOS VM test passes
4. On NixOS with mounted pool: `sudo braid browse` — tabs cycle, subtabs switch, output loads
5. Subvolumes tab: j/k selects, Enter shows detail, Esc goes back
6. `r` reloads, `?` shows help, `q` quits
