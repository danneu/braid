# Plan: document `Shift-R` in the TUI manual; leave help overlay deliberately silent

## Context

The TUI footer advertises `Reset temp hi/lo: R` (cli/src/tui/view/mod.rs:1392),
but the keybinding is missing from `manual/commands/tui.md`'s Keybindings table
-- a real documentation gap left over from PR #54 (commit `43db401`).

A finding pitched fixing this by adding `R` to *both* the manual and the in-app
help overlay (`cli/src/tui/view/help.rs`). That second half is wrong:

- `keymap.rs:16-18` places the uppercase-`R` handler *after* the `show_help`
  guard at `keymap.rs:9-11`, so pressing `R` while help is open is consumed
  by the close-on-any-key handler and never reaches
  `Message::ResetTemperatureStats`.
- Test `keymap.rs:97-103` (`uppercase_r_closes_help_not_reset`) pins this
  invariant.
- The originating plan
  (`plans/impl/2026-04-16-tui-disk-temperature-column.md:39`) explicitly
  designed it that way: "`R` isn't advertised in the help overlay, and
  silently mutating stats while help is visible would be surprising."

Advertising `R` in the overlay would lie: the user reads "R = reset hi/lo",
presses R, help closes, stats are unchanged. So the right shape of the fix is
a pivot from the finding -- update the manual, leave the overlay alone, and
add a tiny inline note in `help.rs` so this same finding doesn't cycle back
in future reviews.

## Files to modify

- `manual/commands/tui.md` -- Keybindings table (lines 36-46).
- `cli/src/tui/view/help.rs` -- add one `//` comment inside `view_help`
  documenting the deliberate omission.

Nothing else changes. No code behavior changes. No snapshot tests are
affected (the footer text is unchanged; the help overlay row list is
unchanged).

## Changes

### 1. `manual/commands/tui.md` -- add one row

Append a `Shift-R` row to the Keybindings table at line 36-46, placed after
the existing `?` row so the in-app reset action sits at the bottom of the
table:

```
| `Shift-R` | Reset session temperature hi/lo watermarks |
```

Style notes (verified against the existing table):
- Use `Shift-R` (backtick, hyphen-separated) to match the existing
  `Shift-Tab` row.
- Plain `R` is rejected because the table already has a lowercase `r` row
  ("Reload pool data"); a separate uppercase `R` row without the `Shift-`
  prefix would read as a typo.
- Action wording mirrors the footer's "Reset temp hi/lo" but spells out
  "watermarks" for a reader without the in-app context. Keep it short --
  cookbook style per AGENTS.md.

### 2. `cli/src/tui/view/help.rs` -- one comment

Inside `view_help` (before the `let lines = vec![...]` block at line 8), add:

```
// `R` (reset temp hi/lo) is intentionally NOT listed here.
// Pressing any key while the help overlay is open is consumed by the
// close-on-any-key handler in keymap.rs, so advertising R here would
// mislead -- users would press R, help would close, and stats would
// remain. The footer in view/mod.rs is the in-app surface for R.
```

This comment passes the "removing it loses non-obvious WHY" bar in
`AGENTS.md`: a reader of `help.rs` alone cannot recover the keymap
interaction from this file's contents.

## Out of scope (deliberately not doing)

- Not changing `cli/src/tui/keymap.rs`. The handler order is intentional.
- Not changing the help overlay's row list. See above.
- Not changing the footer string in `cli/src/tui/view/mod.rs`. Already
  correct; snapshot tests pin it.
- Not touching `README.md` or any other doc -- exploration confirmed no
  other surface advertises the TUI keybindings.

## Verification

1. `just test-rust` -- passes; no Rust code changes other than a comment
   that does not alter behavior, so the keymap tests
   (`r_refreshes_pool_in_disk_detail`,
   `uppercase_r_resets_temperature_stats_in_main`,
   `uppercase_r_resets_temperature_stats_in_disk_detail`,
   `uppercase_r_closes_help_not_reset`,
   `lowercase_r_refreshes_pool_in_main`) all still cover the contract.
2. `just test-rust` also runs the view snapshot suite; verify no
   snapshot diffs (the help overlay's rendered text is unchanged because
   we only added a code comment, and the footer is unchanged).
3. Manual eyeball: open `manual/commands/tui.md` in a renderer and confirm
   the table parses cleanly with the new row.
4. Read-only check: `rg 'Shift-R' manual/` returns exactly one hit
   (the new row).
