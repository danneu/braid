# Plan: pivot `format_runtime` onto the existing `format_duration_secs` helper

## Context

`cli/src/ups.rs:258-271` defines `format_runtime`, which renders a UPS
battery runtime in seconds as either `M:SS` (sub-hour) or `H:MM`
(hour-plus). The split is ambiguous at exact boundaries: 60s and 3600s
both render as `"1:00"`. An operator reading `Runtime: 1:00` cannot tell
whether shutdown is imminent (1 minute) or comfortable (1 hour).

The codebase already has the helper this finding is asking for, a few
feet away in the same TUI: `format_duration_secs` at
`cli/src/tui/view/mod.rs:438-449`, used today for scrub durations and
scrub time-left. It renders the unambiguous unit-suffixed forms
`Xh Ym Zs` / `Xm Ys` / `Xs`. The right work is unification, not adding
a third format style. The TUI is currently rendering the UPS row in
one style (`30:00`) and the scrub row in another (`30m 0s`) in the same
view -- consolidating the two helpers also dissolves that
inconsistency.

Outcome: a single shared duration-rendering helper lives in
`cli/src/util.rs`, `format_runtime` is gone, and every UPS-runtime
surface (human CLI output, TUI sidebar UPS row, TUI browse panel,
manual example, snapshot tests) uses the unambiguous form.

## Files modified

- `cli/src/util.rs` -- add `pub(crate) fn format_duration_secs`.
- `cli/src/ups.rs` -- delete `format_runtime` (lines 258-271) and its
  test `format_runtime_splits_on_hour_boundary` (lines 330-340);
  rewrite the call site at line 199 to use the shared helper with a
  `u32 -> u64` widen.
- `cli/src/tui/view/mod.rs` -- delete the local
  `format_duration_secs` (lines 438-449); update the two scrub
  callers (lines 486, 546) and the UPS-row caller (line 258) to use
  `crate::util::format_duration_secs`. Add one new focused render
  test in the existing `// --- UPS rendering tests ---` block
  (currently around line 2107) that builds a `Model` with
  `ups_config = Some(...)` and `ups = Some(UpsSnapshot { runtime_secs:
  Some(1800), .. })`, renders the Data tab via the existing
  `render` / `buffer_to_string` harness at lines 1432-1452, and
  asserts the rendered buffer contains `"30m 0s"`. Closes the
  Data-tab coverage gap: the demo model used by `snapshot_loading`
  / `snapshot_not_mounted` / `snapshot_with_pool` has
  `ups_config = None` (see `cli/src/tui/model.rs:405`), so the UPS
  row never appears in any committed Data-tab snapshot today.
- `cli/src/tui/browse/view.rs` -- update the UPS panel runtime
  caller at line 315 to use `crate::util::format_duration_secs`
  instead of `crate::ups::format_runtime`.
- `cli/src/snapshots/snapshot_human_online.snap` -- `Runtime: 30:00`
  -> `Runtime: 30m 0s`.
- `cli/src/snapshots/snapshot_human_onbattery.snap` --
  `Runtime: 19:00` -> `Runtime: 19m 0s`.
- `cli/src/snapshots/snapshot_human_lowbattery.snap` --
  `Runtime: 0:45` -> `Runtime: 45s`.
- `cli/src/snapshots/snapshot_human_replace_battery.snap` --
  `Runtime: 30:00` -> `Runtime: 30m 0s`.
- `cli/src/tui/browse/snapshots/snapshot_browse_nut_status.snap:9`
  -- `Runtime  30:00` -> `Runtime  30m 0s` (two-space gap is the
  table padding; preserve column alignment by adjusting trailing
  spaces in the cell).
- `cli/src/tui/browse/snapshots/snapshot_browse_nut_status_multi_flag.snap:9`
  -- same browse-panel runtime update for the multi-flag UPS snapshot.
- `tests/cli/braid-status-ups.py` -- update the assertion at line 26
  and its preceding comment at line 25 to expect `Runtime: 30m 0s`
  for the 1800s case. No other assertions in this file touch the
  runtime string.
- `manual/commands/ups-status.md:23` -- `Runtime: 30:00` ->
  `Runtime: 30m 0s`.
- `manual/guides/ups.md:67` -- `Runtime: 30:00` ->
  `Runtime: 30m 0s`.

No new modules, no renames, no signature change to the helper itself.

## Design notes

- The helper keeps its existing name `format_duration_secs` and its
  existing `u64` parameter. The scrub callers already pass `u64`; the
  UPS caller widens `parsed.battery.runtime_secs` (an `Option<u32>` per
  `cli/src/parse/types.rs`) with `.map(|s| s as u64)` before piping
  into the helper. `u32::MAX` seconds is roughly 136 years, so the
  cast is lossless for any plausible UPS runtime.
- The helper becomes `pub(crate)` in `cli/src/util.rs` rather than
  exported through `ups.rs` or kept in `tui/view/mod.rs`. `util.rs`
  is the established home for cross-module helpers (it currently
  houses `require_tty` and `now_iso`); putting it there matches the
  existing layout and avoids `tui/` importing from a sibling
  module's view internals.
- Per AGENTS.md doc-comment rules, add a `///` line on the relocated
  helper that captures the invariant ("renders a duration in seconds
  with unit suffixes so callers never produce the ambiguous `H:MM`
  vs `M:SS` collision at 3600s"). One to three lines.
- The helper's current step at 3600s emits `1h 0m 0s` (slightly
  verbose because the seconds component sticks even when zero). This
  behavior is preserved as-is to keep the diff small and to avoid
  changing the scrub rows' rendering. A separate cleanup could trim
  trailing-zero components later if desired -- explicitly out of
  scope for this pivot.
- Drop the existing test
  `format_runtime_splits_on_hour_boundary`
  (`cli/src/ups.rs:330-340`) because the function it pins no longer
  exists. Replace it with a small test for
  `format_duration_secs` colocated with the helper in `util.rs`
  pinning the three behavioral branches (`3600 -> "1h 0m 0s"`,
  `60 -> "1m 0s"`, `45 -> "45s"`) and the previously-ambiguous
  boundary (`3599 -> "59m 59s"` vs `3600 -> "1h 0m 0s"` are now
  distinct). This is the first direct unit test for the helper; it
  was previously only exercised through TUI snapshots.
- The new Data-tab render test uses a contains-style assertion
  (`assert!(buf.contains("30m 0s"))`) rather than a full `snap!`
  snapshot. The `snap!` macro pins the entire frame buffer, which
  would lock in layout details unrelated to the formatter and
  produce churn on every TUI tweak. A focused contains-check
  matches the style of the surrounding UPS unit tests
  (`ups_severity_*`) and isolates this test's purpose: prove the
  shared helper reaches the Data-tab UPS row through `ups_section`
  / `format_ups_runtime`.

## Implementation steps

1. Move `format_duration_secs` into `cli/src/util.rs` as
   `pub(crate)`; add the `///` doc line and the four-case unit test
   in a `#[cfg(test)] mod tests` block in `util.rs`.
2. Delete the local copy in `cli/src/tui/view/mod.rs:438-449` and
   point the two scrub callers (lines 486, 546) and the UPS-row
   caller (line 258, currently routing through
   `crate::ups::format_runtime`) at `crate::util::format_duration_secs`.
   Add the new Data-tab render test in the `// --- UPS rendering
   tests ---` block: build a `Model` with `ups_config = Some(...)`
   and `ups = Some(UpsSnapshot { runtime_secs: Some(1800), .. })`,
   call `render(&model, 60, 24)` / `buffer_to_string(...)`, assert
   the rendered string contains `"30m 0s"`.
3. Delete `format_runtime` and its test in `cli/src/ups.rs`; rewrite
   the `format_human` call site (line 199) to widen and forward into
   the shared helper.
4. Update the `cli/src/tui/browse/view.rs:315` caller likewise.
5. Update the snapshot files in place to the new strings:
   - the four `cli/src/snapshots/snapshot_human_*.snap` files
   - `cli/src/tui/browse/snapshots/snapshot_browse_nut_status.snap`
     line 9 (preserve column padding inside the table cell)
   - `cli/src/tui/browse/snapshots/snapshot_browse_nut_status_multi_flag.snap`
     line 9 (same runtime cell in the multi-flag fixture).
6. Update the VM canary assertion + comment at
   `tests/cli/braid-status-ups.py:25-26` to expect
   `Runtime: 30m 0s`.
7. Update both user-doc lines:
   `manual/commands/ups-status.md:23` and
   `manual/guides/ups.md:67`.
8. `cargo fmt` once at the end.

## Verification

- `just test-rust` -- re-pins six snapshot surfaces:
  the four `snapshot_human_*.snap` files exercised from
  `cli/src/ups.rs` and the browse-panel
  `snapshot_browse_nut_status*.snap` files exercised from
  `cli/src/tui/browse/view.rs`. This is the primary regression
  surface. The new `format_duration_secs` unit test in `util.rs`
  pins the helper's branches.
- `just test-vm braid-status-ups` -- the live UPS parser canary
  defined by `tests/cli/braid-status-ups.py`. After updating the
  `Runtime: 30:00` assertion at line 26, this test confirms the
  live CLI output line on a VM with the dummy-ups driver matches
  the updated snapshot string and no other UPS assertions
  regressed.
- The new Data-tab render test in `cli/src/tui/view/mod.rs` covers
  the previously script-blind sidebar UPS row: it builds a `Model`
  with `ups_config` populated, renders through the existing
  `TestBackend` harness, and asserts the rendered buffer contains
  `30m 0s`. Together with the browse-panel snapshot, both TUI
  Runtime cells now have automated coverage.
- Visual check of TUI sidebar and browse panel remains a useful
  smoke step but is no longer the only line of defense for the
  Data tab: the UPS row's Runtime cell should read `30m 0s` (not
  `30:00`), matching the scrub row's style in the same view. Run
  `braid tui` against the test VM if you want eyes on the rendered
  frame.
- `git grep -n "format_runtime" cli/` should return zero matches
  after the change (it is fully deleted). For
  `format_duration_secs`, expect: one definition in
  `cli/src/util.rs`, the new unit test in the same file (multiple
  hits inside the `#[cfg(test)] mod tests` block), and five
  production callers (one in `cli/src/ups.rs`, three in
  `cli/src/tui/view/mod.rs`, one in `cli/src/tui/browse/view.rs`).
  Sanity-check the match list manually rather than pinning an exact
  count.
- No JSON contract change. `runtime_secs: u32` is still emitted raw
  in `--json` output; only the human-rendered surface changes. The
  JSON assertions in `tests/cli/braid-status-ups.py` (`runtime_secs
  == 1800` at line 42) are untouched and continue to pass.

## Out of scope

- Trimming trailing-zero components from `format_duration_secs`
  (e.g. `1h` instead of `1h 0m 0s` at exact hour boundaries) -- a
  separate cleanup that would also touch scrub-row rendering.
- Switching to `HH:MM:SS` (the upstream NUT `upsstats.c` convention)
  -- also unambiguous, but it would diverge from the project's
  existing scrub-duration style. Consistency wins.
- Any change to `parsed.battery.runtime_secs`'s storage type or to
  the JSON shape. The pivot is rendering-only.
