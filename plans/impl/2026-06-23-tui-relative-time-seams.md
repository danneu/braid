# Plan: testable seams for TUI relative-time rendering (UTC-skew regression guard)

## Context

A code-review finding (Testing/Medium) flagged that the TUI's relative-time
rendering has **zero unit coverage**, and the "naive-local `now`" basis
correction that prevents UTC skew is **untestable** because it lives inline in
the event loop.

Why this matters: btrfs emits scrub `ctime` in host-local wall-clock, and
`parse_ctime` (`cli/src/parse/helpers.rs#parse_ctime`) returns it as a naive
`PrimitiveDateTime`. For the "N min ago" text to be correct, `now` must share
that naive-local basis. `run_loop` does this today
(`cli/src/tui/mod.rs:103-107`) by resolving the host offset and stripping it --
but a reintroduced UTC basis, or a sign error in the offset application, would
skew every relative time by the host's offset and **trip no test lane**. The
negative-diff branch of `timeago` (which silently erases the "(... ago)"
suffix) is likewise uncovered.

The finding's proposed test -- "feed a `now` derived from a non-UTC offset into
the scrub-row formatter" -- does **not**, as written, guard the regression it
names: `timeago`/`scrub_terminal_rows` contain no offset logic (they subtract
two already-naive datetimes), so a test that constructs `now` itself would keep
passing even if `run_loop` reverted to UTC. **The pivot:** extract `run_loop`'s
offset projection into a pure helper *owned by `cli/src/tui/mod.rs`* -- in
`run_loop`'s module, not a downstream `view` helper, per the test bar
(`docs/dev/testing.md#regression-test-quality`: "test the layer where production
failed, not a downstream parser or helper that only proves later code works when
given correct input"). **Be precise about what the helper's test pins:** the
projection's *internal correctness* -- offset sign, fractional (`:30`) offsets,
UTC passthrough -- plus an *in-place* revert of the helper's body to a UTC basis.
It does **not** pin `run_loop`'s choice to call the helper: a future
"simplification" that bypasses it (inlining `PrimitiveDateTime::new(now_utc().date(),
now_utc().time())`) leaves `frame_local_now` correct in isolation, so the test
still passes and no lint backstops it (`[workspace.lints.clippy]` sets only
`result_large_err`; the test keeps the helper `used`). That whole-block bypass is
the same untestable OS-boundary class as `now_utc()`/`current_local_offset()`
themselves -- a host-independent unit test cannot exercise the real offset, and
VM checks run on macOS -- so it is listed in the residue (see Risks), not claimed
as covered. The extraction still earns its keep: it pins the projection
arithmetic, gives the basis invariant a named, documented seam, and mirrors the
testable-time precedent `cli/src/membership.rs#format_rfc3339_utc_seconds`
(wall-clock passed as a parameter; unit-tested at `membership.rs:844`).
Separately, extract the scrub "Last run" compose logic so its suffix-drop /
`None`-erase branches get exact-output coverage.

Intended outcome: every `timeago` branch and the projection arithmetic are
pinned by host unit tests (no VM -- macOS VM checks cannot exercise a
deterministic non-UTC offset). The untestable surface is the inline OS
clock/offset acquisition in `run_loop` **and** `run_loop`'s choice to apply the
offset (the whole-block bypass) -- same OS-boundary class, documented as residue.

## Production changes (behavior-preserving; lift logic verbatim)

Two pure seams plus a `run_loop` rewire. The frame-time seam moves to
`cli/src/tui/mod.rs` (owned by `run_loop`); the scrub-display seam stays in `view`.
**Do not "tidy" comparisons (`>` vs `>=`) or `format!` spacing while lifting** --
`snapshot_scrub_tab` pins the resulting strings with no behavioral assert, so
any whitespace change is a snapshot diff. The goal is byte-identical output.

1. **Extract `frame_local_now`** into `cli/src/tui/mod.rs` (the module that owns
   `run_loop`) -- the pure projection test 1 pins:
   ```rust
   /// The naive-local wall-clock `now` each frame renders against. Scrub ctime
   /// (parse_ctime) is naive-local; a UTC-basis `now` would skew `timeago` by the
   /// host's offset. Pure (utc + offset passed in) so the projection is unit-tested
   /// directly without depending on the host timezone.
   fn frame_local_now(
       utc: time::OffsetDateTime,
       offset: time::UtcOffset,
   ) -> time::PrimitiveDateTime {
       let local = utc.to_offset(offset);
       time::PrimitiveDateTime::new(local.date(), local.time())
   }
   ```
   Private `fn` (same module as `run_loop`; that module's test reaches it directly).

2. **Rewire `run_loop`** at `cli/src/tui/mod.rs:103-107`:
   ```rust
   // current_local_offset() is sound despite the multithreaded TUI: time >= 0.3.37
   // dropped the old "fail when multithreaded" rule and calls localtime_r directly,
   // so unwrap_or(UTC) guards only a genuine localtime failure, not thread count.
   let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
   let now = frame_local_now(time::OffsetDateTime::now_utc(), offset);
   ```
   Comment split: the **naive-local-basis** rationale (`tui/mod.rs:96-99`) moves
   to the `frame_local_now` `///`; the **multithreaded-soundness** rationale
   (`tui/mod.rs:100-102`) stays inline above the `offset` binding, where it
   explains the `unwrap_or(UTC)` that is *not* moving. The two OS-call inputs
   (`current_local_offset()` + `now_utc()`) stay inline and untested -- the
   irreducible boundary (cf. `SystemTime::now()` in the `membership.rs`
   precedent). Targeting `frame_local_now` puts the guard at `run_loop`'s seam,
   as close to the production-failure layer as a host-independent test reaches.

3. **Extract `scrub_last_run_display`** from `scrub_terminal_rows`
   (`cli/src/tui/view/mod.rs:516-522`), lifted verbatim:
   ```rust
   /// The Scrub "Last run" cell: absolute timestamp plus a relative suffix,
   /// with the suffix dropped when `now` precedes `started_at` (clock skew) --
   /// see `timeago`. `None` started_at renders "unknown".
   fn scrub_last_run_display(
       started_at: Option<&ScrubTimestamp>,
       now: PrimitiveDateTime,
   ) -> String {
       match started_at {
           Some(ts) => match timeago(&ts.0, now) {
               Some(ago) => format!("{} ({})", format_timestamp(&ts.0), ago),
               None => format_timestamp(&ts.0),
           },
           None => "unknown".to_owned(),
       }
   }
   ```
   `scrub_terminal_rows` sets `let display = scrub_last_run_display(started_at, now);`.

Optional, on-theme: add a one-line `///` to `timeago` (currently undocumented).
Discretionary since it is private. **Do not** generalize the `render()` test
helper to `render_at(now)` -- speculative, and it would touch the shared helper
used by ~dozens of snapshots.

## Test changes

Split by the layer each guards -- the projection test sits at the `run_loop`
seam, the rest in `view`. Each test gets the project's `// Intent / Why it exists /
Scenario` preamble. Use the `time::macros::offset!(-06:00)` / `offset!(+05:30)`
literal macro (compile-time, const, matches the file's `datetime!` style) rather
than `UtcOffset::from_hms`. Output strings are ASCII ("min ago", "day ago",
"unknown") -- compliant.

### New `#[cfg(test)] mod tests` in `cli/src/tui/mod.rs`

1. **`frame_local_now_projects_to_host_wall_clock`** -- feed
   `datetime!(2026-02-24 12:00:00 UTC)` (an `OffsetDateTime`) with `offset!(-06:00)`,
   `offset!(+05:30)`, and `UtcOffset::UTC`; assert naive results
   `datetime!(2026-02-24 06:00:00)`, `17:30:00`, `12:00:00`. Pins the projection's
   internal correctness: an offset **sign flip** (the `-06:00` case would read
   `18:00:00` not `06:00:00`), a **fractional** (`:30`) offset, **UTC passthrough**,
   and an **in-place body revert** to a UTC basis (would read `12:00:00`). It does
   **not** pin `run_loop`'s choice to *call* the helper -- the whole-block bypass is
   residue (see Risks), not covered here.

### Existing `#[cfg(test)] pub(crate) mod tests` in `cli/src/tui/view/mod.rs`

(already has `use super::*;` and reaches private fns.)

2. **`timeago_buckets_and_future_none`** -- with `now = datetime!(2026-02-24 12:00:00)`:
   - 3 days prior -> `"3 days ago"` (multi-day)
   - **47h prior -> `"1 day ago"`** (the `days==1`/`days==2` truncation cliff --
     catches a `>` vs `>=` flip in the `days > 1` branch)
   - exactly 24h prior -> `"1 day ago"` (singular boundary)
   - 30 min prior -> `"30 min ago"` (general past)
   - 30s prior -> `"<1 min ago"` (sub-minute)
   - **exactly `now` (diff 0) -> `"<1 min ago"`** (explicit named assert -- the
     boundary between `None` (future) and sub-minute; the likely off-by-one if
     the negative guard is rewritten as `<=`)
   - `now + 1s` (future) -> `None` (annotation erased)

3. **`scrub_last_run_shows_timestamp_and_ago`** -- `Some(started_at)` in the past
   -> display is `"<timestamp> (<n> min ago)"` (locks the compose for the normal
   branch; exact-output per the render-boundary bar).

4. **`scrub_last_run_drops_suffix_on_future_started_at`** -- future `started_at`
   -> display has no `"ago"` but still shows the absolute timestamp (the
   negative-diff `None`-erase branch).

5. **`scrub_last_run_none_is_unknown`** -- `None` -> `"unknown"`.

Tests 3-5 cover `scrub_last_run_display`'s suffix/drop/unknown branches only; the
projection arithmetic is pinned by test 1 at the `run_loop` seam (the whole-block
bypass stays residue, see Risks), not chained through this downstream consumer of
`now`.

## Critical files

- `cli/src/tui/mod.rs` -- add `frame_local_now`, rewire `run_loop`'s `now`, split
  the comment, add a `#[cfg(test)] mod tests` with the projection test (test 1).
- `cli/src/tui/view/mod.rs` -- extract `scrub_last_run_display`, optional `///` on
  `timeago`, add tests 2-5 to the existing module.
- Reference only (no change): `cli/src/parse/helpers.rs#parse_ctime`,
  `cli/src/parse/types.rs` (`ScrubTimestamp`), `cli/src/membership.rs:622/844`
  (precedent), `cli/Cargo.toml:23` (`time` features:
  `formatting, macros, parsing, local-offset` -- all required APIs present, no
  new feature).

## Verification

- `just test-rust` -- the new tests run on the host (macOS); the offset is
  injected as a parameter, so no VM and no real non-UTC host needed.
- `just clippy` -- doc-comment + lint pass on the new items.
- `just check-output-ascii` -- the repo's ASCII-output guard over
  `cli/src/**/*.rs` (`cargo clippy` does not run this).
- Confirm **no snapshot churn**: `snapshot_scrub_tab`,
  `snapshot_scrub_tab_with_errors`, and friends all render through `render()`'s
  hardcoded `now = datetime!(2026-02-24 02:12:00)`, and the extractions are
  byte-identical -- there should be zero pending insta snapshots
  (`cargo insta test` / no `.snap.new`).

## Risks / notes

- **Visibility:** `frame_local_now` is a private `fn` in `tui/mod.rs`, called by
  `run_loop` in the same module and reached by that module's test -- no new
  `pub`. `scrub_last_run_display` stays private in `view/mod.rs`.
- **Untestable residue (by design), two parts:** (1) the inline OS-call inputs
  `current_local_offset()` + `now_utc()`; and (2) `run_loop`'s *choice to apply
  the offset* -- a whole-block revert that bypasses `frame_local_now` (inlining a
  naive-UTC `now`) leaves the helper correct in isolation, so test 1 still passes
  and no lint backstops it (`[workspace.lints.clippy]` = only `result_large_err`;
  the clippy lane is plain `cargo clippy --tests`; the test keeps the helper
  `used`, so `dead_code` stays quiet). Both are the same OS-boundary class as
  `SystemTime::now()` in the `membership.rs` precedent: a host-independent unit
  test cannot exercise the real offset, and VM checks run on macOS. The named,
  documented `frame_local_now` seam (a dev simplifying `run_loop` meets the basis
  `///`) is the mitigation, not a guarantee. Do not mock the OS offset.
- **Considered, declined -- a `LocalFrameTime` newtype:** making `frame_local_now`
  the sole constructor of a `now` newtype would turn the bypass into a compile
  error and "every `now` reaching the view is local-basis" into a type invariant.
  Declined as disproportionate: it threads a wrapper through `view` /
  `view_scrub` / `scrub_table` / `scrub_terminal_rows` / `timeago` and needs
  module-privacy ceremony to seal the field, for a render-path timestamp. The
  documented seam + residue note is the proportionate call; escalate to the
  newtype only if the bypass actually recurs.
- **Snapshot safety hinges on verbatim lifting** -- the one behavioral trap is
  changing `>`/`>=` or `format!` spacing during extraction; keep them identical.
