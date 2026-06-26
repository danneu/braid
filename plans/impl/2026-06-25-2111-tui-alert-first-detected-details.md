# Plan: surface per-cause first-detected detail in the TUI alert banner

## Context

Commit `03a042ef` (`feat(alert): record first-detected timestamp on alert
latches`) made every latched alert cause carry a required `detected_at`
(RFC3339 UTC seconds) and surfaced it in `braid status` -- both the human
render (`  - missing device: disk2 (devid 2) -- first detected
2026-06-25T15:35:54Z (2 hours ago)`) and `--json`. That work explicitly
deferred the TUI: the live dashboard's alert region is a single hardcoded
banner line (` CRITICAL alert -- pool health issue detected. Run 'braid ack'
... `) that reads `alert_causes` only to compute max severity and never
enumerates the causes, so `first_detected` has nowhere to land. The plan
marked TUI rendering "include if cheap"; it was not cheap, so it shipped as a
Follow Up.

This change closes that Follow Up: render a per-cause detail section beneath
the TUI alert banner that mirrors `braid status` exactly -- one line per cause
with its label, absolute first-detected timestamp, and relative age -- so the
TUI dashboard answers "what is wrong and since when" the moment you open it on
a NAS that has been degraded for hours or days.

The guiding constraint: the TUI must **not diverge** from `braid status`. Both
surfaces render the same cause from the same data, so the label text and the
"first detected ... (age)" suffix become shared, single-source helpers rather
than copy-pasted match arms.

## Two real subtleties (why this is not a trivial render)

1. **Clock basis.** The TUI frame clock `now: PrimitiveDateTime` is naive
   *local* wall-clock (used by scrub `timeago`, which is correct because btrfs
   scrub timestamps are also local). But `first_detected` is RFC3339 *UTC*.
   Subtracting a UTC instant from a local wall-clock yields an age off by the
   local UTC offset. So the alert age needs a genuine UTC `now`, threaded
   separately. `tui::mod::run_loop` already computes
   `time::OffsetDateTime::now_utc()` each frame (it feeds `frame_local_now`),
   so the UTC clock is already in hand -- it just is not passed to `view`.

2. **Cause labels need a devid -> name map.** `braid status` renders
   `missing device: disk2 (devid 2)` via `devid_to_name(devid_names, devid)`;
   without the map it degrades to `missing device: devid 2`. The TUI's
   `probe_pool_for_tui` *already* builds a local `devid_to_name:
   HashMap<Devid, &str>` (`cli/src/tui/probe.rs`, used to key `device_errors`
   by name) -- it is simply not stored on the TUI `PoolState`. Stashing an
   owned copy reuses data already computed, so labels match `braid status`.

## Design

### 1. Extract two shared, single-source render helpers

- **`AlertCause::describe(&self, devid_names: Option<&HashMap<Devid, String>>)
  -> String`** in `cli/src/alert.rs`. Returns the cause label with **no**
  leading `  - ` and **no** first-detected suffix -- exactly the strings
  currently produced inline by the match in `status::format_status_human`:
  - `BtrfsDeviceErrors { devid }` -> `btrfs device errors on {name}`
  - `MissingDevice { devid }` -> `missing device: {name}`
  - `SmartdAlert` -> `SMART health warning`
  - `ScrubFailed` -> `scheduled scrub failed -- check journalctl -u braid-scrub.service`
  - `ComputationError { detail }` -> `alert computation error: {detail}`
  - `EnospcRisk { .. }` -> `ENOSPC risk: pool is one disk-loss from being unable to restore RAID1 redundancy`

  Move the private `devid_to_name` helper from `cli/src/status.rs` into
  `cli/src/alert.rs` as a private helper for `describe` (its only callers are
  the two match arms `describe` replaces; confirmed no other `devid_to_name(`
  callers in status.rs). Add `use std::collections::HashMap;` to alert.rs.

  Update the `ScrubFailed` variant's doc comment in `cli/src/alert.rs`, which
  currently reads "the human text lives in `status.rs#format_status_human`".
  After this change the label lives in `AlertCause::describe`, so the comment
  must point there to stay truthful (doc-comment convention). Sweep the other
  variants' doc comments for the same stale citation while editing.

- **`AlertCauseReport::first_detected_suffix(&self, now: time::OffsetDateTime)
  -> String`** in `cli/src/status.rs`. Returns `""` when `first_detected` is
  `None`; otherwise ` -- first detected {ts}`, plus ` ({age})` when the stamp
  parses (`util::parse_rfc3339_utc`) and is in the past
  (`util::humanize_ago(now - parsed)` returns `Some`). This is exactly the
  current inline suffix logic in `format_status_human`, including the
  fail-loud-don't-crash behavior (future/garbage stamp degrades to
  absolute-only). Keep the explanatory comment with it.

Both helpers are `pub(crate)` and get `///` doc comments (intent: single
source so TUI and `braid status` cannot diverge).

### 2. Rewire `status::format_status_human` onto the shared helpers

Replace the per-cause match + inline suffix with:

```
let mut line = format!("  - {}", c.cause.describe(devid_names));
line.push_str(&c.first_detected_suffix(now.into())); // now: SystemTime -> OffsetDateTime
line.push('\n');
out.push_str(&line);
```

Output must stay byte-identical. The existing status tests
(`alert_missing_device_uses_devid_names_map`,
`alert_*` human-render tests around `format_status_human`) are the regression
guard and must pass unchanged -- do not modify them.

### 3. Give the TUI `PoolState` a `devid_names` map

- Add field `pub devid_names: HashMap<Devid, String>` to the TUI `PoolState`
  in `cli/src/tui/model.rs` with a `///` doc comment.
- Populate it in `cli/src/tui/probe.rs#probe_pool_for_tui` from the existing
  local `devid_to_name` map: `devid_to_name.iter().map(|(k, v)| (*k,
  v.to_string())).collect()`. Place the field in the returned `PoolState {
  ... }` literal.
- Default it to `HashMap::new()` in `cli/src/tui/demo.rs#sample_pool` (and any
  other TUI `PoolState` literal). Empty map => labels fall back to `devid N`,
  which is acceptable for the demo; alert-detail tests inject specific entries.

### 4. Thread a UTC clock into `view` and render the detail section

- Add a parameter `now_utc: time::OffsetDateTime` to
  `cli/src/tui/view/mod.rs#view`. Document the invariant that `now` (local)
  and `now_utc` are the same instant.
- `cli/src/tui/mod.rs#run_loop`: capture `let now_utc =
  time::OffsetDateTime::now_utc();`, feed it to the existing
  `frame_local_now(now_utc, offset)`, and pass `now_utc` to `view`.
- In `view`, change the alert region from a fixed single line to a
  variable-height block:
  - Compute the alert cause list once from `model.pool.current()`. Build a
    `Vec<Line>`: line 0 is the existing severity banner (unchanged text +
    bg-styled span); then one styled line per cause:
    `format!("  - {}{}", c.cause.describe(Some(&pool.devid_names)),
    c.first_detected_suffix(now_utc))`, with `fg` = the severity color
    (`Color::Red` for Critical, `Color::Yellow` for Warning), no background,
    not bold -- visually subordinate to the bold banner.
  - `alert_height = 1 + causes.len() as u16` when active (was `1`). Push one
    `Constraint::Length(alert_height)` (still a single constraint entry, so the
    downstream `off` / `outer[off + 2]` indexing is unchanged), and render the
    whole `Vec<Line>` as one `Paragraph` into `outer[off]`.

  No cap on cause count (mirrors `braid status`, which is also unbounded; real
  pools have a handful of devices). Note it as accepted behavior.

## Files to modify

- `cli/src/alert.rs` -- add `AlertCause::describe`; move in `devid_to_name`;
  add describe unit tests.
- `cli/src/status.rs` -- add `AlertCauseReport::first_detected_suffix`; rewire
  `format_status_human`; remove the now-moved `devid_to_name`.
- `cli/src/tui/model.rs` -- add `PoolState.devid_names`.
- `cli/src/tui/probe.rs` -- populate `devid_names`.
- `cli/src/tui/demo.rs` -- default `devid_names` in `sample_pool`.
- `cli/src/tui/mod.rs` -- thread `now_utc` through `run_loop` into `view`.
- `cli/src/tui/view/mod.rs` -- `now_utc` param; variable-height alert render;
  new tests + helper.
- `docs/commands/tui.md` (`## What it shows`) -- **required**: document that
  the alert banner now lists each cause with its first-detected timestamp and
  relative age. `docs/commands/status.md#alert-banner` already documents the
  `-- first detected <RFC3339 UTC> (<relative age>)` line format (landed in
  commit `03a042ef`), so cross-reference it rather than restate the wording;
  `status.md` needs no edit unless that shared phrasing moves. Keep `README.md`
  in sync per AGENTS.md if it describes the TUI alert region.

## Tests (TDD -- write first, confirm red, then implement)

Behavioral, structure-insensitive assertions only:

1. **`cli/src/alert.rs` -- `describe` unit tests:** each variant's exact label,
   with a name present in the map (`missing device: disk2 (devid 2)`) and
   absent (`missing device: devid 2`). Pins the moved label strings at their
   source.
2. **`cli/src/status.rs` -- existing human-render tests pass unchanged.** These
   already assert the full `  - ... -- first detected ... (N ago)` line; they
   prove the refactor preserves output. (No new status tests required.)
3. **`cli/src/tui/view/mod.rs` -- per-cause detail rendering** (via
   `render(&model, w, h)` + `buffer_to_string`, styles dropped so assert text):
   - Critical alert with a timestamped `MissingDevice` cause: output contains
     the banner AND a `  - missing device: ...` line AND `first detected` AND
     the relative age.
   - **Clock-basis guard:** set the test's local `now` and `now_utc` to
     *different* wall-clock values (simulate a non-UTC offset), inject a
     `first_detected` aligned to UTC two hours before `now_utc`, and assert
     `2 hours ago` appears. If the impl wrongly used the local clock the age
     would go negative and be omitted -- so this assertion fails closed on the
     bug. Add a test helper `pool_with_timestamped_alert(Vec<(AlertCause,
     Option<String>)>)` (mirror of the existing `pool_with_alert`) that also
     seeds `devid_names`.
   - Bridge cause (`EnospcRisk`, `first_detected: None`): its line renders the
     label with **no** `first detected` suffix.
   - One snapshot test (`snap!`) capturing the full multi-line alert region.
   - Layout sanity: with multiple causes the tab body / footer still render
     (no panic, banner + a known body string both present).
4. Update `render()` in the view tests to pass a fixed `now_utc`
   (`time::macros::datetime!(... UTC)`), distinct from the fixed local `now`.
5. **`cli/src/tui/probe.rs` -- `probe_pool_for_tui` populates `devid_names`:**
   sibling to the existing
   `device_errors_for_missing_devid_use_persisted_prior_binding` /
   `device_errors_keyed_by_devid_not_path` tests, drive `probe_pool_for_tui`
   with a mounted pool plus a persisted missing/null-underlying devid and
   assert the returned `PoolState.devid_names` maps that devid to the member
   name. This pins the **live** population path: without it the view tests
   (which inject `devid_names` directly) could stay green while the real probe
   returns an empty map, silently falling back to `devid N` and diverging from
   `braid status`.

## Verification

- `cargo test` for the CLI crate: new alert/describe, status (unchanged),
  TUI view tests, and the existing suite all green. Review any `insta`
  snapshot diffs and accept intentionally.
- `cargo run -- tui` against a demo/degraded pool (or the demo model) to eyeball
  the banner + per-cause lines and confirm the age reads sanely.
- `python3 scripts/docs/check-output-ascii.py` -- new label/suffix strings are
  ASCII (they already are; the helpers just relocate them).
- `just docs-build` (mdbook + linkcheck) if docs touched.
- No NixOS VM tests are involved (TUI rendering is unit/snapshot-tested in
  Rust).

## Out of scope / non-goals

- No change to the alert model, latch format, monitor, `resolve_alert_state`,
  or `--json` output. This is presentation only.
- No new colors/theme work beyond reusing the existing severity colors.
- No interactive per-cause drill-down; the detail is static text under the
  banner.
