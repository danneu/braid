# Plan: fold `braid browse` into `braid tui` as a Browse tab; drop the Sharing placeholder

## Context

`cli/src/browse/` and `cli/src/tui/` carry mechanically duplicated TUI scaffolding -- byte-identical `InputHandler`, frame-loop constants, key-kind filter, and the matching `release_q_is_ignored` / `press_and_repeat_q_emit_quit` tests. Recent history shows the cost: `471dc78` and `a70515f` both touched the two files in lockstep. The duplication only exists because the subvolume browser shipped as its own top-level command before the tui's tab system was rich enough to absorb it.

The right structural fix is not to extract a shared runtime (cleanup that's about to get thrown away). It's to delete `braid browse` outright and add its functionality as a new `Browse` top tab inside `braid tui`. This also lets us drop the never-implemented `Sharing` placeholder tab while we're touching `Tab::ALL`.

Browse is positioned as the **raw CLI output inspector**: low-level, complete, pass-through. Top tabs (`Data`, `Scrub`, future polished panels) are **curated/parsed/first-class** with specialized features (start/pause, ETAs, history). Features get promoted from Browse to top-level when they earn dedicated UX. Overlap between Browse and curated tabs is by design, not duplication.

## Settled design

**Top tab strip after this PR:** `Data | Scrub | Browse` -- 3 tabs.

**Inside the Browse tab:** 3-region sidebar layout

```
+- Browse -----------------------------+
| Pgm col |  Cmd col   |  Content      |
|  Btrfs >| Filesystem | <output>      |
|  NUT    | Devices    |               |
|         | Subvols    |               |
|         | Scrub      |               |
|         | Balance    |               |
+---------+------------+---------------+
```

Plus a conditional 4th column (subview col) whenever the column-2 selection has subviews. Current cases:

- `Btrfs > Filesystem` -> `Usage / Show / Df`
- `Btrfs > Devices` -> `Usage / Stats` (preserves `btrfs device stats` error-counter inspection that current `braid browse` exposes via `SubTab::DevStats` at `cli/src/browse/model.rs:58, 142-147`)

The column appears/disappears dynamically; content area widens when it's gone. Columns 2 selections without subviews (`Subvolumes`, `Scrub`, `Balance`, all NUT commands) skip column 4 entirely and render content directly in column 3.

**v1 inventory (column 1):** `Btrfs`, `NUT` -- two rows. LUKS / SMART / Systemd land in later iterations by appending rows.

**Per-program commands (column 2):**

- `Btrfs`: `Filesystem` (with Usage / Show / Df subviews), `Devices` (with Usage / Stats subviews), `Subvolumes`, `Scrub`, `Balance`
- `NUT`: `Status` (curated rollup, same data the Data tab's UPS section uses), `Variables` (raw `upsc <name>` stdout), `Commands` (raw `upscmd -l <name>` stdout)

**Keymap inside Browse tab:**

- `Tab` / `BackTab` -- cycles top tabs (unchanged from rest of `braid tui`)
- `h` / `l` -- moves focus across sidebar regions (Program <-> Command <-> [Subview] <-> Content)
- `j` / `k` -- moves selection within current focused region
- Selecting in a sidebar region immediately drives content (no `Enter` needed for sidebar)
- `Enter` -- drills into a row in the content area (e.g. subvolume detail)
- `Esc` / `Backspace` -- pops content drill-down back to row list
- `r` -- reloads current command output
- `?` -- toggles help overlay
- `q` -- quits
- `Ctrl-D` / `Ctrl-U` -- page down/up in content

**Empty-state UX (no auto-spawn on gated paths):**

- **Pool offline (`model.pool.current().is_none()`)**: column 1 row `Btrfs` stays selectable; column 2 menu populates. Content area for any Btrfs command shows `pool not mounted -- run \`braid unlock\` to access btrfs data`. **No `Effect::BrowseRunCommand` is emitted** for Btrfs branches in this state -- the empty state replaces the spawn.
- **NUT not configured (`model.ups_config.is_none()`, which is the case in `Model::new_demo` at `cli/src/tui/model.rs:397`)**: column 1 row `NUT` stays visible-and-selectable so the menu remains discoverable; column 2 menu populates. Content area shows `UPS not configured -- set \`ups.name\` in the braid NixOS module`. No NUT effects fire.
- Both states must be checked by the centralized loader (see `BrowseState::load_current` in Created section) -- not scattered across update arms.

**Single PR.** braid is unreleased; CLAUDE.md forbids compat shims. `braid browse` and `cli/src/browse/` disappear in the same change. No alias, no `--browse` flag on `braid tui`.

## Critical files

### Created

- `cli/src/tui/browse/mod.rs` -- module roots + re-exports
- `cli/src/tui/browse/state.rs` -- `BrowseState`, `BrowseFocus`, `BrowseProgram`, `BrowseCommand`, `FilesystemSubview`, `DeviceSubview`, `BrowseEmptyState` enums; per-command output cache with generation counters; the central scheduler:
  - `BrowseState::load_current(&mut self, pool: &PoolStatus, ups_config: Option<&crate::config::Ups>) -> Option<Effect>` -- bumps the generation for the current `(program, command, subview)` tuple and returns the `Effect::BrowseRunCommand` to emit, OR returns `None` after installing the matching `BrowseEmptyState` (`PoolOffline` when Btrfs branch + pool absent; `UpsNotConfigured` when NUT branch + `ups_config.is_none()`). This is the single source of truth for "what should the content area be showing right now" -- every call site routes through it.
- `cli/src/tui/browse/keymap.rs` -- key dispatch invoked from `tui::keymap` only when `ctx.tab == Tab::Browse`; emits new `BrowseMsg`-shaped variants of `Message`
- `cli/src/tui/browse/view.rs` -- renders the 3-or-4 region layout; per-command content renderers (or delegates to shared helpers in `cli/src/tui/view/mod.rs`)
- `cli/src/tui/browse/snapshots/*.snap` -- new insta snapshots (see Tests below)
- `docs/decisions/025-browse-vs-curated.md` (`Active`) -- captures the layering distinction so the next person reading the code doesn't ask "isn't Scrub already a tab?"

### Modified

- `cli/src/tui/model.rs` (L11-43)
  - Remove `Tab::Sharing` variant; replace position with `Tab::Browse`
  - Update `Tab::ALL`, `Tab::label`, `Tab::next`, `Tab::prev` (final order: `Data -> Scrub -> Browse -> Data`)
  - Add `pub browse: BrowseState` field to `Model` (L268-299)
  - Initialize in `Model::new` and `Model::new_demo` (L346, L378)
- `cli/src/tui/app.rs` (Message enum L11-38; `update` fn at L60-75 for `NextTab`/`PrevTab`)
  - Add Message variants: `BrowseFocusLeft`, `BrowseFocusRight`, `BrowseSelectNext`, `BrowseSelectPrev`, `BrowseEnter`, `BrowseBack`, `BrowseReload`, `BrowseCommandFinished { raw, generation }`, `BrowseScrollDown`, `BrowseScrollUp`, `BrowsePageDown`, `BrowsePageUp`
  - **Centralize Browse loading through `BrowseState::load_current`** (defined in `cli/src/tui/browse/state.rs`). The following arms must call it and propagate its returned `Option<Effect>`:
    - `Message::NextTab` / `Message::PrevTab` -- after `model.tab = model.tab.next()` (or `prev`), if the new tab is `Tab::Browse`, call `model.browse.load_current(&model.pool, model.ups_config.as_ref())`. This fixes the current behavior at `cli/src/tui/app.rs:69-75` where `NextTab` emits `vec![]` and would leave Browse blank on first entry.
    - `Message::BrowseSelectNext` / `Message::BrowseSelectPrev` -- after the sidebar selection mutates, call `load_current` to (re)schedule the new selection's command.
    - `Message::BrowseReload` -- call `load_current` (it bumps the generation, which invalidates any in-flight response).
  - `BrowseCommandFinished` is generation-checked before being applied; mismatches are dropped silently (same pattern as `cli/src/browse/app.rs:159`).
  - Drill-in (`BrowseEnter` in a subvolume row) follows the existing `cli/src/browse/app.rs:121-152` pattern: cache the list output, generation-bump, fire `Effect::BrowseRunCommand` with `BtrfsSubvolumeShow`.
- `cli/src/tui/effect.rs` (L18-50)
  - Add `Effect::BrowseRunCommand { request: CmdRequest, generation: u64 }`
  - Add `execute_effect` arm spawning a worker thread (mirror the existing pattern at `cli/src/browse/mod.rs:140-162`)
- `cli/src/tui/event.rs` (L17-30, `into_message` signature)
  - Add `Event::BrowseCommandFinished { raw, generation }`
  - **Replace `into_message(self, show_help: bool, show_disk_detail: bool)` with `into_message(self, ctx: &KeyContext)`** where `KeyContext` is a new struct (defined alongside `handle_key` in `cli/src/tui/keymap.rs`) carrying at minimum:
    ```
    pub struct KeyContext {
        pub tab: Tab,
        pub show_help: bool,
        pub show_disk_detail: bool,
        pub browse_focus: BrowseFocus, // only consulted on Tab::Browse
    }
    ```
    The two existing `event.into_message(model.show_help, model.show_disk_detail)` call sites at `cli/src/tui/mod.rs:109,113` become `event.into_message(&KeyContext { tab: model.tab, show_help: model.show_help, show_disk_detail: model.show_disk_detail, browse_focus: model.browse.focus })`.
- `cli/src/tui/keymap.rs` (L1-38, `handle_key` signature)
  - **Change `handle_key(key: KeyEvent, show_help: bool, show_disk_detail: bool)` to `handle_key(key: KeyEvent, ctx: &KeyContext)`** (signature at `cli/src/tui/keymap.rs:5`).
  - **Preserve the existing help-overlay-swallows-everything contract** at `cli/src/tui/keymap.rs:6-11`. The required key-handling order is:
    1. `Ctrl-C` -> `Message::Quit` (must beat help; same as today at `cli/src/tui/keymap.rs:6-8`).
    2. `if ctx.show_help { return Some(Message::ToggleHelp); }` -- help swallows ALL other keys including `q`, `Tab`, `BackTab`, `R`, `?`, `h`, `l`, `j`, `k`, `Enter`, `Esc`, `r`. This is the contract pinned by `cli/src/tui/keymap.rs:90-100` (`uppercase_r_closes_help_not_reset`). Do not break it.
    3. Non-help globals (apply regardless of `ctx.tab`): `q` -> `Quit`, `?` -> `ToggleHelp`, `Tab` -> `NextTab`, `BackTab` -> `PrevTab`, `R` -> `ResetTemperatureStats`.
    4. Then branch on `ctx.tab`:
       - `Tab::Browse`: delegate to `crate::tui::browse::keymap::handle_key(key, ctx)` which interprets `h`/`l`/`j`/`k`/`Enter`/`Esc`/`r`/`Ctrl-D`/`Ctrl-U` against `ctx.browse_focus`. Returns `Option<Message>` of `Browse*` variants only.
       - `Tab::Data` / `Tab::Scrub`: existing dispatch at `cli/src/tui/keymap.rs:27-37` (arrow keys + `j`/`k` move disk selection, `Enter` opens disk detail, `r` -> `RefreshPool`, `show_disk_detail` handling).
- `cli/src/tui/view/mod.rs`
  - Tab dispatch around L1394-1397: drop `Tab::Sharing` arm; add `Tab::Browse => crate::tui::browse::view::view_browse(model, frame, area)`
  - Remove `view_placeholder` if its only caller was Sharing (grep first)
  - Snapshot tab-bar rendering will pick up the new `Tab::ALL` automatically
- `cli/src/main.rs`
  - Delete the `Browse(BrowseArgs)` subcommand variant (L69-70)
  - Delete `BrowseArgs` struct (L290-298)
  - Delete dispatch arm (L929-952) -- ~24 lines
- `cli/src/cmd.rs`
  - Add `CmdRequest::UpscmdList { name: String }` for Browse > NUT > Commands; mirror `UpscQuery` dispatch at L251-253, L833-836
- `cli/src/tui/model.rs` (`UpsSnapshot` definition at L295 / line near the existing `pub ups: Option<UpsSnapshot>` field)
  - Add `pub raw_text: String` to `tui::model::UpsSnapshot` so Browse > NUT > Variables can render the unparsed `upsc <name>` stdout without a second fetch. **`UpsSnapshot` is a tui-only type; `UpscOutput` (in `cli/src/parse/types.rs`) is the parser output and is NOT extended** -- keeping the parse model untouched avoids accidentally widening the `braid ups status --json` surface (which serializes via `cli/src/ups.rs::JsonReport`).
- `cli/src/tui/probe.rs` (`probe_ups_for_tui` at L741-760)
  - **Inline the raw fetch in the probe; do not modify `cli/src/ups.rs::query_ups`.** Replace the `query_ups(runner, name)` call with a direct `runner.run(&CmdRequest::UpscQuery { name })`, capturing `raw.stdout` for the new `UpsSnapshot.raw_text` field, then calling `parse_upsc(&raw.stdout)` to build the parsed half of the snapshot. This keeps `cli/src/ups.rs::query_ups` and its `braid ups status --json` caller (at `cli/src/ups.rs:42`) unchanged.
- `cli/src/lib.rs` (L4)
  - Delete `pub mod browse;`. Leaving this declaration in place after the `cli/src/browse/` directory is removed is a compile-break -- the deletion commit must remove the declaration in the same commit.
- `cli/src/util.rs` (L7-13 hint branch; L34, L41-58 tests)
  - Delete the `if cmd == "browse"` branch in `require_tty_inner` and the `let hint = ...` plumbing -- the only remaining caller is `tui` (and `tui --demo` re-uses `"tui"`), so the function reduces to `Err(io::Error::other(format!("braid {cmd} requires a terminal")))`.
  - Update the comment on `require_tty_inner_blocks_when_either_stdio_is_not_a_tty` (the `"--check" hint for browse only` note) and delete the four `require_tty_inner("browse", ...)` assertions that reference `braid browse requires a terminal -- use --check for non-interactive mode`. The remaining `"tui"` assertions stay.
- `cli/tests/tty_guard.rs`
  - Delete the `"browse" => braid_cli::browse::run("/mnt/storage")` arm in `maybe_run_probe` and the `browse_rejects_non_tty_stdio` test; the `tui` / `tui_demo` cases remain (they're the surviving entry points and exercise the same guard)
- `flake.nix:146-150` -- delete `braid-browse = pkgs.testers.nixosTest (...)` block
- `justfile:101` -- remove `braid-browse` from the `test-parsers` argument list
- `README.md` -- replace any `braid browse` examples with `braid tui` Browse-tab equivalents
- `manual/index.md:47` -- delete the row linking to `commands/browse.md`
- `manual/SUMMARY.md:37` -- delete the bullet linking to `commands/browse.md`
- `manual/commands/tui.md:61` -- drop the `**Sharing** -- placeholder (coming soon).` bullet; replace with a new `**Browse** -- raw CLI output inspector ...` entry describing the new tab
- `manual/commands/tui.md:66` -- drop or replace the link to `browse.md`; the new Browse tab is documented in-line on this page
- **Do NOT touch** `manual/guides/sharing-and-permissions.md`, `manual/index.md:24`, or `manual/SUMMARY.md:16` -- those are about Samba/storage-group permissions and are unrelated to the `Tab::Sharing` placeholder being dropped.
- `docs/index.md` -- add the new decision doc to the directory

### Deleted

- `cli/src/browse/` (entire directory: `mod.rs`, `app.rs`, `event.rs`, `keymap.rs`, `model.rs`, `view.rs`, `snapshots/`)
- `tests/cli/braid-browse.nix`
- `tests/cli/braid-browse.py`
- `manual/commands/browse.md`
- `command-findings/browse.md`
- `features1/feature-findings/browse.md`

## Patterns to reuse

These already exist in `cli/src/browse/` and `cli/src/tui/`; the new code should port them rather than re-invent.

- **Generation-based staleness** for in-flight commands -- `cli/src/browse/app.rs:159, 191`. Every sidebar selection bumps `command_gen`; `BrowseCommandFinished` is dropped if its generation doesn't match.
- **Per-command `CmdRequest` mapping** -- `cli/src/browse/model.rs:131-158`. The new `BrowseCommand` enum carries the same shape (`Filesystem(FilesystemSubview)`, `Devices`, `Subvolumes`, `Scrub`, `Balance`, etc.) and returns its `CmdRequest`.
- **Subvolume detail drill-in** -- `cli/src/browse/app.rs:121-152`. The list-output cache + generation invalidation pattern transfers wholesale.
- **Spinner animation** -- `cli/src/browse/view.rs:11`; `model.frame` is already on `tui::Model`.
- **`section_block`, `disk_table`** etc. in `cli/src/tui/view/mod.rs:94, 102, 739, 393, 322` -- Browse can call these for shared visual style.
- **`require_tty`** -- already wired in `cli/src/tui/mod.rs:28, 69`; nothing to change here.
- **Existing UPS probe path** -- `cli/src/tui/probe.rs:741-760` + `cli/src/ups.rs::query_ups`. Browse > NUT > Status reads `model.ups` (the existing `UpsSnapshot`); Variables reads `model.ups.as_ref().map(|s| &s.raw_text)`; Commands fires `Effect::BrowseRunCommand { request: CmdRequest::UpscmdList { name } }` lazily on first selection and caches the output for the session.

## Tests

### Snapshot tests (new, under `cli/src/tui/browse/snapshots/`)

- `snapshot_browse_default` -- initial state, focus on Program col, Btrfs > Filesystem > Usage by default
- `snapshot_browse_btrfs_filesystem_usage` (and `_show`, `_df` for the 3 subviews)
- `snapshot_browse_btrfs_devices_usage`, `_devices_stats` (the two Devices subviews, mirroring current `braid browse` parity)
- `snapshot_browse_btrfs_subvolumes`, `_subvolume_detail`, `_scrub`, `_balance`
- `snapshot_browse_nut_status`, `_variables`, `_commands`
- `snapshot_browse_pool_offline` -- empty-state in content when pool unmounted
- `snapshot_browse_focus_command_col`, `_focus_subview_col`, `_focus_content`

### Snapshot regeneration (existing)

All 24 `.snap` files under `cli/src/tui/view/snapshots/` render the tab bar as `Data  Scrub  Sharing`. After dropping Sharing + adding Browse, regenerate with `INSTA_UPDATE=auto cargo test -p braid-cli` (or `cargo insta accept` after a normal run). Review the diff to confirm only the tab strip changed.

### Keymap tests

In `cli/src/tui/keymap.rs::tests` (top-level router):

- `tab_is_global_across_all_tabs` -- builds a `KeyContext` with `tab: Tab::Browse, browse_focus: BrowseFocus::Content, show_help: false`, sends a `Tab` keypress, asserts `Message::NextTab` is returned (proves Browse focus doesn't swallow the global). Repeat for `BackTab`, `Ctrl-C`, `?`, `R`.
- `browse_keys_only_route_on_browse_tab` -- with `tab: Tab::Data`, send `h`, assert it does NOT produce a `BrowseFocusLeft` (it either no-ops or routes to the Data-tab dispatch). With `tab: Tab::Browse`, send `h`, assert `BrowseFocusLeft` is returned.
- `data_tab_keys_unchanged` -- regression coverage: `j`/`k`/`Enter` on `Tab::Data` still produce `SelectNextDisk`/`SelectPrevDisk`/`OpenDiskDetail` exactly like `cli/src/tui/keymap.rs:33-36` does today.
- `help_swallows_q_tab_r_h_l` (regression) -- with `show_help: true`, send each of `q`, `Tab`, `BackTab`, `R`, `?`, `h`, `l`, `j`, `k`, `Enter`, `Esc`, `r`; every one must return `Some(Message::ToggleHelp)`, NOT `Quit`/`NextTab`/`PrevTab`/`ResetTemperatureStats`/`BrowseFocusLeft`/etc. This pins the help-overlay-swallows-everything contract from `cli/src/tui/keymap.rs:9-11` and extends it to the new global + Browse-local key surface. The existing `uppercase_r_closes_help_not_reset` at `cli/src/tui/keymap.rs:90-100` is a subset of this; keep or fold in.
- `ctrl_c_still_quits_inside_help` -- regression: `show_help: true` + `Ctrl-C` returns `Quit`, not `ToggleHelp`. `Ctrl-C` is the one global that beats help.

In `cli/src/tui/browse/keymap.rs::tests`:

- `h_at_leftmost_is_noop` / `l_at_rightmost_is_noop`
- `l_from_command_skips_subview_when_no_subviews` -- e.g. `Btrfs > Subvolumes` selected; `l` skips column 4 (no subviews) and lands on content
- `l_from_command_enters_subview_when_filesystem` -- `Btrfs > Filesystem` selected; `l` lands on the Usage/Show/Df column
- `l_from_command_enters_subview_when_devices` -- `Btrfs > Devices` selected; `l` lands on the Usage/Stats column (parallel coverage for the second subview-bearing command)
- `j_in_program_cycles_btrfs_nut`
- `j_in_subview_cycles_filesystem_usage_show_df` / `j_in_subview_cycles_devices_usage_stats`
- `enter_in_subvolume_row_drills_in` / `esc_pops_back`

In `cli/src/tui/event.rs::tests` (the surviving canonical pair):

- `release_q_is_ignored` / `press_and_repeat_q_emit_quit` -- updated to construct a `KeyContext` instead of the two-bool args; the `cli/src/browse/event.rs` copy disappears with the deleted module.

In `cli/src/tui/browse/state.rs::tests` (the new central loader):

- `load_current_on_btrfs_offline_pool_returns_none_and_sets_empty_state` -- asserts `BrowseEmptyState::PoolOffline` is installed and no effect is returned when `pool.current().is_none()`.
- `load_current_on_nut_without_config_returns_none_and_sets_empty_state` -- asserts `BrowseEmptyState::UpsNotConfigured` is installed when `ups_config.is_none()`.
- `load_current_bumps_generation_and_returns_effect` -- happy path.
- `next_tab_into_browse_emits_effect` -- integration: in `app.rs::update` with `tab` transitioning Data -> Scrub -> Browse, `NextTab` returns an `Effect::BrowseRunCommand` (not `vec![]`).

### VM tests

**Same-PR parser-canary replacement is required.** `braid-browse` is part of the parser-compatibility canary contract documented in `AGENTS.md:211` -- it's the only live VM coverage of `parse_btrfs_subvolume_list` (CLI-reachable parser, not in the TUI-only exclusions at `AGENTS.md:213`). Dropping it without replacement shrinks the parser-canary surface.

Ship `tests/cli/braid-tui-browse.{nix,py}` in this PR. **PTY-driven `braid tui` only** -- this PR is specifically about moving Browse into `braid tui`, so the canary must exercise the real integration. A non-UX shim (`braid debug subvolume-list` or similar) would (a) introduce a new command surface the rest of the plan doesn't need, and (b) leave the Browse tab's actual code paths untested. The VM test must:

1. Boot a fixture pool (mirror `tests/cli/braid-browse.nix`'s setup: RAID1, unlocked, with a known subvolume created post-mount).
2. Launch `braid tui` **as root** on a known virtual console (e.g. tty2), with stdin/stdout/stderr bound to `/dev/tty2`. Non-demo `braid tui` is root-gated at `cli/src/main.rs:376-380` (`Commands::Tui(args) if args.demo => false` is the only carve-out; everything else exits early with `error: braid must be run as root` when `geteuid() != 0`), and the manual documents the live invocation as `sudo braid tui` at `manual/commands/tui.md:15`. The old `braid-browse` canary likewise ran as root via `machine.succeed` at `tests/cli/braid-browse.py:15`. The cleanest harness pattern is a small NixOS module overlay defining a `systemd.services.braid-tui-canary` (or a one-shot on `tty2` via `agetty --autologin root --skip-login --login-program /run/current-system/sw/bin/braid -- tui`) -- implementer's choice, as long as the process runs as uid 0 with `/dev/tty2` as its controlling terminal.
3. **Switch the active console to tty2 before sending keys** -- `machine.send_key("alt-f2")` or `machine.execute("chvt 2")`. Without this, `send_key` is routed to whichever VT is currently foreground (typically tty1), and the keystrokes never reach `braid tui`. Then wait for the TUI to render (a `wait_until_tty_matches("2", r"Data\s+Scrub\s+Browse")` against the tab strip is a good readiness signal). The kitty-protocol filter at `cli/src/tui/event.rs:36-39` accepts `KeyEventKind::Press`, which is what the harness sends.
4. Navigate `Tab` -> `Tab` to land on Browse (Data -> Scrub -> Browse) so `BrowseState::load_current` is exercised on tab entry.
5. Send `j` to highlight `Btrfs > Subvolumes` (or use the default selection plus `l` to focus column 2 and `j` until Subvolumes is reached -- exact sequence depends on default focus).
6. Assert the known subvolume name appears in the rendered terminal buffer via `machine.wait_until_tty_matches("2", r"<subvol-name>")` -- this reads `/dev/vcs2` (the kernel's visible-terminal-buffer for tty2) directly, no OCR. This pins `parse_btrfs_subvolume_list` to live `btrfs subvolume list` output.
7. Press `Enter` to drill into subvolume detail; assert a stable line from `btrfs subvolume show` via `machine.wait_until_tty_matches("2", r"<subvol-uuid-or-name-regex>")`. Pick a field that doesn't drift across runs (UUID, Name, or the literal section header) -- generation numbers change.
8. Press `Esc` to pop back to the list; reassert the subvolume name with `wait_until_tty_matches` to confirm the list view re-renders.

**Do NOT add `enableOCR = true;` to the test config.** `wait_until_tty_matches` reads the VT character buffer at `/dev/vcs<N>` via the kernel; no screenshotting or Tesseract is involved. The canary's job is to prove the parser + Browse-tab wiring works against live btrfs-progs output, not to verify visual rendering.

Register the new test in `flake.nix` (the slot previously occupied by `braid-browse` at L146-150) and add it to `justfile:101`'s `test-parsers` list.

The other VM canaries (`braid-status-rust`, `braid-status-ups`, etc.) cover the remaining parser surface and stay untouched.

### Unit tests

- `cli/src/parse/upsc.rs` -- if `UpsSnapshot` schema changes, update fixture-based tests at `cli/tests/fixtures/nixos-25.11/upsc/`. The new `raw_text` field is the input itself, so no parser change.

## Verification

End-to-end checklist before merge:

1. **Build clean** -- `cargo check -p braid-cli` and `cargo clippy -p braid-cli` pass with zero warnings.
2. **Rust unit + snapshot tests** -- `just test-rust` green. Confirm `cli/src/tui/view/snapshots/` shows tab strip = `Data  Scrub  Browse` and the Browse snapshots exist.
3. **Static surface (browse)** -- `grep -rEn 'braid browse|braid_cli::browse|crate::browse|browse\.md|^pub mod browse|^mod browse|browse --check' manual/ README.md docs/ cli/ tests/ flake.nix justfile` returns zero hits outside historical plan docs under `plans/impl/`. Specifically:
   - `cli/src/lib.rs:4` no longer declares `pub mod browse;`.
   - `cli/src/util.rs` `require_tty_inner` no longer carries the `cmd == "browse"` branch and the `braid browse --check` hint string.
   - `manual/index.md`, `manual/SUMMARY.md`, and `manual/commands/tui.md` no longer link to `commands/browse.md`.
4. **Static surface (Sharing tab)** -- `grep -rn 'Tab::Sharing\|"Sharing"\b' cli/ tests/` returns zero hits. (The `Sharing and permissions` Samba guide at `manual/guides/sharing-and-permissions.md` is unrelated and stays.)
5. **VM canary** -- `just test-parsers` green; the suite includes the new `braid-tui-browse` registered in `flake.nix` at the slot vacated by `braid-browse` (L146-150) and in `justfile:101`'s argument list. `just test-vm braid-status-rust braid-status-ups braid-idle braid-discover braid-tui-browse` green.
6. **Demo TUI smoke** -- `cargo run -p braid-cli -- tui --demo` opens, `Tab` cycles to Browse (and Browse-local `h`/`l`/`j`/`k` do NOT trigger spurious top-tab cycling), `j`/`k` moves within a region, selecting `Btrfs > Filesystem` reveals the conditional Usage/Show/Df column. Because demo mode has `ups_config: None`, Browse > NUT > any-command must render the `UPS not configured` empty state -- not a blank panel or panic. Confirm `Tab` from inside Browse cycles back to Data.
7. **Live TUI smoke (NAS)** -- `sudo braid tui` on a mounted pool with a configured UPS: every Btrfs command in Browse > Btrfs returns output; `r` reloads; Subvolumes drill-in works via Enter / Esc; Browse > NUT > Variables renders raw upsc text; Browse > NUT > Commands renders `upscmd -l` output.
8. **Empty-state UX** -- two separate runs:
   - locked pool: Browse > Btrfs shows the `pool not mounted` empty state; Browse > NUT still polls and renders (assuming ups_config is set).
   - no `ups_config`: Browse > NUT shows the `UPS not configured` empty state; Browse > Btrfs still works (assuming pool is mounted).
9. **Tab-entry auto-load** -- from `braid tui` cold start with the user on `Data`, pressing `Tab` twice lands on `Browse` and the content area is populated within one frame (not blank). Verifies the `NextTab` -> `load_current` wiring.
10. **TTY guard** -- `cargo test -p braid-cli --test tty_guard` green: `tui` and `tui_demo` arms still reject redirected stdio; the deleted `browse` arm is gone.
11. **Docs sweep** -- `README.md`, `docs/index.md`, `manual/index.md`, `manual/SUMMARY.md`, and `manual/commands/tui.md` updated; `manual/commands/browse.md` deleted; new `docs/decisions/025-browse-vs-curated.md` linked from `docs/index.md`. Final grep over `manual/ README.md docs/` shows zero `braid browse`, `commands/browse.md`, or `Tab::Sharing` mentions.

## Suggested commit sequence (single PR)

The implementer can break this into commits for review-ability; each should land green:

1. `refactor(tui): drop unused Sharing placeholder tab` -- removes `Tab::Sharing` and regenerates the 24 affected snapshots
2. `feat(tui): add Browse top tab scaffolding (empty)` -- adds `Tab::Browse`, empty `view_browse`, `BrowseState` skeleton
3. `feat(tui): wire Browse 3-region sidebar layout + h/l/j/k navigation`
4. `feat(tui): wire Browse > Btrfs > Filesystem (with Usage/Show/Df subview column)`
5. `feat(tui): wire Browse > Btrfs > Devices, Subvolumes, Scrub, Balance`
6. `feat(tui): extend UpsSnapshot with raw_text for Browse > NUT > Variables`
7. `feat(tui): wire Browse > NUT > Status, Variables, Commands` (includes `CmdRequest::UpscmdList`)
8. `test(tui): snapshot + keymap tests for Browse tab` (includes the `tab_is_global_across_all_tabs` / `browse_keys_only_route_on_browse_tab` / `next_tab_into_browse_emits_effect` regression coverage)
9. `test(parsers): add braid-tui-browse VM canary` -- replacement for the about-to-be-deleted `braid-browse` parser canary; must land before commit 10 so `just test-parsers` is never red on the branch
10. `chore: delete cli/src/browse, braid browse command, braid-browse.{nix,py}, manual/commands/browse.md` (also removes `pub mod browse;` from `cli/src/lib.rs:4`; drops the `cmd == "browse"` branch + the four `browse`-asserting tests in `cli/src/util.rs:7-13, 41-58`; unregisters `braid-browse` from `flake.nix:146-150` and `justfile:101`; replaces the entry in justfile's `test-parsers` list with `braid-tui-browse`). This commit only lands green after commit 9 (the VM canary replacement).
11. `docs: add decision 025-browse-vs-curated; update README, docs/index, manual/index, manual/SUMMARY, manual/commands/tui`

## Open decisions (deferable to implementation)

- **Default focus on first Browse tab entry**: Program column (`Btrfs` highlighted) feels right -- gives the user a recognizable starting point and j/k immediately works.
- **Default sub-selections**: `Btrfs > Filesystem > Usage`, `NUT > Status`.
- **Auto-refresh interval inside Browse**: none for v1. Manual `r` only. NUT > Status piggybacks on the existing 5s UPS polling via `model.ups`.
- **upscmd -l fetch strategy**: lazy on first selection of Browse > NUT > Commands; cache for session lifetime (per-UPS-model output is essentially static).
