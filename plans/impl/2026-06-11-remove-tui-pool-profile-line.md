# Remove the TUI Pool "Profile" one-liner

## Context

The TUI home screen's Pool section renders a `Profile    data RAID1 | meta RAID1 | system RAID1`
line directly above the allocation table (`Type / Profile / Used / Allocated`). Both are computed
from the same `pool.df_entries`, so the line is a strict projection of the table's first two
columns. The only things it adds are two summary words: `partial` (a block-group type spans
multiple profiles) and `unknown` (a type returned no entries) -- and the underlying facts remain
visible as table rows either way. The user reviewed that trade-off and chose to remove the line
outright (no replacement highlight, no conditional rendering). Goal: less vertical noise in the
Pool section, one fewer redundant rendering path.

`braid status` (CLI) keeps its own profile summary -- `cli/src/profile_summary.rs` and
`status.rs#format_type_profile_human` are untouched by this change.

## Changes

All code changes are in `cli/src/tui/view/mod.rs`:

1. **`pool_info`** -- delete the Profile block: the `profile_summary::from_df_entries` call and
   the `if !profile_summary_is_empty(&profile) { lines.push(...) }` that renders the
   `Profile    data X | meta Y | system Z` line.

2. **Delete now-dead helpers** (all private to this file, no other callers):
   - `format_type_profile_tui`
   - `profile_summary_is_empty`
   - `pool_profile_rows`

3. **Imports** -- remove `use crate::profile_summary::{self, Redundancy, TypeProfile};`
   (no other use in this file; verify with `cargo check`).

4. **Layout height math** -- two call sites sum `pool_profile_rows(p)` into the Pool section
   height; drop the term and fix the adjacent comment ("border + Path + optional Profile +
   balance + usage + blank + header + entries" -> remove "optional Profile"):
   - `view_data` (the `pool_height` calculation)
   - the second `info_rows = 1 + pool_profile_rows(pool) + pool_balance_rows(pool) + usage_row`
     site (fans-section layout)

5. **Tests** (in the same file's `mod tests`):
   - **Repurpose `tui_pool_info_mixed_data`**: it currently guards the `partial` rendering;
     keep it as the guard that the allocation table renders a mixed state as two `Data` rows
     (single + RAID1) -- the only remaining surface for degraded-redundancy visibility in the
     TUI. Rewrite its `// Intent / Why it exists / Scenario` preamble accordingly
     (form per `docs/dev/testing.md`).
   - **Delete** the tests whose sole intent was the deleted formatting fn, plus
     `snapshot_mixed_data_profile` (an older preamble-less test that renders the same mixed
     pool as the repurposed `tui_pool_info_mixed_data` and would duplicate it post-change):
     - `tui_pool_info_3disk_raid1`
     - `tui_pool_info_single_disk`
     - `tui_pool_info_missing_type_renders_unknown`
     - `tui_pool_info_unrecognized_profile_renders_verbatim`
     - `snapshot_mixed_data_profile`
   - Keep the `df_entry` / `pool_with_df_entries` test helpers (still used by the repurposed
     test).
   - **Snapshot inventory** -- `rg -l "Profile    " cli/src/tui/view/snapshots` finds 22
     tracked `.snap` files containing the line. Split:
     - **Deleted** with their tests (5): `tui_pool_info_3disk_raid1.snap`,
       `tui_pool_info_single_disk.snap`, `tui_pool_info_missing_type_renders_unknown.snap`,
       `tui_pool_info_unrecognized_profile_renders_verbatim.snap`,
       `snapshot_mixed_data_profile.snap`.
     - **Regenerated** via insta (17): `snapshot_balance_running.snap`,
       `snapshot_balance_unknown.snap`, `snapshot_disk_detail.snap`,
       `snapshot_disk_detail_null_underlying.snap`, `snapshot_disk_detail_nvme.snap`,
       `snapshot_fans_section_active.snap`, `snapshot_fans_section_daemon_failed.snap`,
       `snapshot_fans_section_no_drives.snap`, `snapshot_fans_section_no_hardware.snap`,
       `snapshot_fans_section_pre_probe.snap`, `snapshot_footer_duration_after_spinner.snap`,
       `snapshot_footer_spinner_inflight.snap`, `snapshot_stale_banner.snap`,
       `snapshot_temperature_column.snap`, `snapshot_with_advisory.snap`,
       `snapshot_with_pool.snap`, `tui_pool_info_mixed_data.snap`.
     Accept each updated snapshot after eyeballing that only the `Profile    ` line
     disappeared (plus the one-row-shorter Pool section).

## Docs and comments

- `docs/commands/tui.md`, "What it shows" -> "Main view" paragraph: remove the `Profile`
  summary description (the parenthetical explaining `data <X> | meta <Y> | system <Z>`,
  `partial`, and `unknown`), leaving pool status, mount point, capacity bar, balance state,
  alerts/advisories. ASCII only.
- `cli/src/profile_summary.rs` becomes status-only after this change; its comments currently
  name the TUI as a co-consumer. Update so the module's stated reason to exist is the
  `braid status` human/JSON profile surfaces:
  - `ProfileSummary` doc comment ("for `braid status` and `braid tui`" / "CLI and TUI
    classification stay in sync").
  - `Redundancy` doc comment ("human and TUI render suffixes").
  - Test preambles that cite the TUI as a consumer (the "status and TUI must agree" /
    "human, TUI, and JSON surfaces" / "status or TUI asks for a summary" / "while the TUI"
    lines).
- `docs/book/` is built output -- do not edit; README does not mention the line.

## Verification

1. `cargo check` in `cli/` -- confirms the removed import and helpers leave no dangling
   references and no new dead-code warnings.
2. Run the Rust test suite (`just test-rust`); review insta snapshot diffs -- each affected
   snapshot should differ only by the missing `Profile    ` line (and the Pool section being
   one row shorter, which the layout-math change in step 4 must absorb; a height mismatch
   would show up as a clipped or padded Pool section in the snapshots).
3. Eyeball the live TUI via the demo path (`braid tui` demo mode used by `Model::new_demo`,
   or `just`-provided run recipe) to confirm the Pool section renders tight with no blank row
   where the line used to be.

## Implementation notes

- Verification step 3 (eyeball the live demo TUI) was covered by reviewing the 17
  insta snapshot diffs instead: they render the same `Model::new_demo` pool through
  the full `view()` path, and each diff showed the Pool section tight with no blank
  row where the Profile line used to be. No interactive run (no TTY in this session).
- `cargo insta accept` was broken locally (cargo-insta linked against a
  garbage-collected nix libiconv), so the reviewed `.snap.new` files were accepted by
  renaming them over their `.snap` baselines -- the same operation `accept` performs.
