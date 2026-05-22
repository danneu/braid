# Make `q` close the topmost modal before quitting the TUI

## Context

In the braid TUI, `q` always exits the program -- even when the disk-detail
popup is open. The only way to dismiss the popup is Esc or Backspace. This is
out of step with the prevailing TUI convention and is a latent footgun for the
confirm dialogs that braid will grow around destructive LUKS/btrfs operations.

Research on popular TUIs (gitui, tig, ranger, yazi, neovim) shows the dominant
pattern when modals are common: **`q` closes the topmost modal if one is open,
and only quits the program when nothing is on top.** Esc remains a universal
close. Ctrl+C is the unconditional emergency quit. The cautionary tale is
gh-dash (dlvhdr/gh-dash#355), which has the same bug braid has today.

This plan adopts the gitui model with a minimal, focused change.

## Current behavior

`cli/src/tui/keymap.rs:17-36` dispatches:

- `Ctrl+C` -> `Quit` (early return, beats everything).
- `show_help` -> `ToggleHelp` (early return, any key closes help).
- Then a top-level `match key.code` where `'q' -> Quit` is matched *before*
  the disk-detail guard inside `handle_data_key`. Net result: `q` quits even
  with the popup open.

The help overlay is already correct because of the early `show_help` return.
Only the disk-detail popup has the bug today, but the fix should be shaped so
future modals slot in cleanly.

## Change

### `cli/src/tui/keymap.rs`

Replace the unconditional `'q' -> Quit` arm in `handle_key` with a
context-aware version:

```rust
KeyCode::Char('q') => {
    if ctx.show_disk_detail {
        Some(Message::CloseDiskDetail)
    } else {
        Some(Message::Quit)
    }
}
```

Leave `?`, `Tab`, `BackTab`, and `R` as global keys that fire even when
disk-detail is open. The existing tests (`uppercase_r_..._in_disk_detail`,
`tab_is_global_across_all_tabs`) document this intent.

Keep the existing `Esc | Backspace -> CloseDiskDetail` arm inside
`handle_data_key` -- it stays the standard close path. We are only widening
the close-popup vocabulary by one key.

### `cli/src/tui/view/help.rs:38-41`

Update the help listing so users learn the new binding. The column is
fixed-width 8 chars; `q/<esc> ` fits exactly:

```rust
Line::from(vec![
    Span::styled("q/<esc> ", Style::default().fg(Color::Cyan)),
    Span::raw("close detail"),
]),
```

Leave the top `q       quit` line as-is -- it is still accurate for the
no-modal case, which is when help is most commonly opened.

### `cli/src/tui/view/mod.rs:1146`

Update the disk-detail popup footer hint:

```rust
Line::from(Span::styled("r reload · q/Esc to go back", dim)),
```

This change will diff three insta snapshots
(`snapshot_disk_detail.snap`, `snapshot_disk_detail_null_underlying.snap`,
`snapshot_disk_detail_unmounted_mixed.snap`). Accept via `cargo insta review`.

## Tests

Add to the `tests` module in `cli/src/tui/keymap.rs`:

- `q_closes_disk_detail_not_app` -- with `show_disk_detail=true`,
  pressing `q` returns `Message::CloseDiskDetail`, not `Quit`. This is the
  direct regression test for the reported bug.
- `q_quits_when_no_modal_open` -- with all modal flags false, `q` returns
  `Message::Quit`. Explicit guard against a future refactor accidentally
  swallowing the quit path.
- `q_closes_help_overlay_when_stacked_over_detail` -- with
  `show_help=true` *and* `show_disk_detail=true`, `q` returns
  `Message::ToggleHelp`. The view renders disk detail first then help on
  top (`cli/src/tui/view/mod.rs:1406-1412`), so help is the topmost
  modal when stacked; pinning the stacked case guards against a future
  refactor that closes the underlying detail while help remains
  visible.

The existing tests (`r_refreshes_pool_in_disk_detail`,
`uppercase_r_resets_temperature_stats_in_disk_detail`,
`help_swallows_q_tab_r_h_l`, `ctrl_c_still_quits_inside_help`) all stay
green unchanged.

Follow the `// Intent / // Why / // Scenario` preamble convention from
AGENTS.md.

## Future modals

Each new modal (e.g. a confirm-destroy dialog) adds a `show_<x>` flag to
`KeyContext` in `cli/src/tui/keymap.rs:10-15` and extends the `q` arm.
**Order matters: check the topmost modal first**, so `q` always closes
the layer the user is currently looking at, not a lower one. A confirm
dialog opened above disk detail must be checked before
`show_disk_detail`:

```rust
KeyCode::Char('q') => {
    if ctx.show_confirm_destroy { Some(Message::CloseConfirmDestroy) }
    else if ctx.show_disk_detail { Some(Message::CloseDiskDetail) }
    else                         { Some(Message::Quit) }
}
```

The rendering order in `view/mod.rs` is the source of truth for what is
on top -- mirror it in the keymap arm. Help is already handled by the
early `show_help` return at the top of `handle_key`, which correctly
makes help win against any underlying modal.

Don't pre-build an abstraction for "topmost modal" -- with one modal
today the conditional is clearest. Revisit only when the chain grows
past 2-3.

## Verification

1. `just test-rust` -- runs the new and existing keymap unit tests.
   The three disk-detail snapshot tests will fail and emit `.snap.new`
   files (per `docs/tui-insta-guide.md`).
2. `cargo insta review` -- inspect the diffs and accept the three
   disk-detail snapshots (footer text change only).
3. `just test-rust` again -- prove the accepted snapshots pass and the
   suite is fully green before the manual smoke.
4. Manual smoke test in `braid tui` (or `braid tui --demo` if that flag
   exists):
   - On the Data tab, press `Enter` on a disk to open the detail popup.
   - Press `q` -- expect the popup to close and the TUI to remain running.
   - Press `q` again -- expect the TUI to exit.
   - Press `?` to open help, press `q` -- expect help to close (unchanged
     behavior, sanity check).
   - Press `Ctrl+C` from any state -- expect immediate exit (unchanged).
