# Plan: surface `PoolStatus::ErrorStale` via a global TUI banner

## Context

`PoolStatus::ErrorStale(String, PoolState)` was introduced in commit `c44dead`
("tui: keep stale data during re-probe") so a failed re-probe keeps the
previously-known pool data on screen instead of blanking it out. The
discriminator was deliberately chosen over `Error(msg)` precisely so both the
error AND the data remain available.

The error half is currently dropped on the floor:

- `cli/src/tui/view/mod.rs:986` (`view_data`) destructures
  `PoolStatus::ErrorStale(_, pool)` -- the message is discarded.
- `cli/src/tui/view/mod.rs:1075` (`view_scrub`) does the same.
- `PoolStatus::is_inflight()` returns `false` for `ErrorStale`
  (`cli/src/tui/model.rs:229-231`), so the footer spinner stops and the reload
  string falls through to `Reload: r ({}ms)` -- visually identical to a
  successful refresh (`cli/src/tui/view/mod.rs:1365-1374`).
- `model.advisories` (`cli/src/tui/mod.rs:32`) is populated once at startup
  with LUKS header-backup advisories. It is not a runtime channel.

Net effect: a transient probe failure -- e.g. a `btrfs` command spawn
failure (`cli/src/tui/probe.rs:41,48`) or a `parse_btrfs_df_json` /
`parse_btrfs_device_usage` failure (`cli/src/tui/probe.rs:42,49`) -- leaves
the user staring at stale data with no visible indication that the latest
refresh failed. (Note: smartctl, scrub status, and balance probes all
degrade gracefully via `.ok()` / `.unwrap_or(...)` in
`cli/src/tui/probe.rs:100-103,107-119`, so smartctl flakes do NOT produce
`ErrorStale`; only hard pool-probe failures do.)

The originating finding proposed a "stale -- last refresh failed: {msg}" line
prepended to each section that handles `ErrorStale`. This plan pivots to a
single top-of-screen banner because:

1. The pool-probe failure spans every pool-derived section -- the Pool box,
   the Disks table, and the Scrub box all render data harvested in a single
   `ProbePool` effect, so a `PoolProbeFinished(Err(...))` (`cli/src/tui/app.rs:141-170`)
   invalidates all three. A per-section line under-signals that scope and
   forces every current and future pool-derived tab to opt in.
2. The existing `alert_active` banner at `cli/src/tui/view/mod.rs:1304-1336`
   establishes the pattern: a conditional 1-line strip spliced into the
   global `Layout::vertical` via `Constraint::Length(1)` and an `off` offset.
   Reusing it gives one source of truth.
3. The banner remains visible on the Scrub tab and any future tab that
   reuses `PoolStatus`, without each tab having to opt in.

**Scope: pool data only.** `ErrorStale` is created exclusively by
`PoolProbeFinished` (`cli/src/tui/app.rs:167-170`). The Fan and UPS
sections live on independent state paths -- `FanProbeFinished`
(`cli/src/tui/app.rs:180`) and `UpsProbeFinished`
(`cli/src/tui/app.rs:210`) write their own `model.fan` / `model.ups`
snapshots, and their probe failures surface as `DaemonStatus` in their
own section headers. A pool-probe failure does not stale-ify Fan or UPS
data, so the banner wording must be scoped to pool data rather than
saying the whole UI is stale.

Outcome: when a re-probe fails, the user sees a yellow banner reading
`" pool data stale -- last pool refresh failed: {msg} "` above the tab
bar, regardless of which tab is active. The footer spinner/duration line
is left untouched.

## Critical files

- `cli/src/tui/model.rs` -- add a `stale_error(&self) -> Option<&str>`
  accessor on `PoolStatus`, paralleling the existing `current()` and
  `is_inflight()` helpers (`cli/src/tui/model.rs:219-232`).
- `cli/src/tui/view/mod.rs` -- splice the new banner into `pub fn view(...)`
  between the advisories row and the tab bar, then add the view-level
  snapshot test.
- `cli/src/tui/app.rs` -- no production change, but add one unit test
  in the existing `mod tests` block that pins the
  `Mounted -> ErrorStale` transition through `PoolProbeFinished(Err)`.

No production changes to `view_data` (`cli/src/tui/view/mod.rs:984-1006`),
`view_scrub` (`cli/src/tui/view/mod.rs:1073-1080`), `is_inflight`, the
footer reload string, or the `cli/src/tui/app.rs` update loop. The
existing `ErrorStale(_, pool)` destructures stay -- they're correct,
they just don't need to surface the message any more.

## Implementation

### 1. Add accessor on `PoolStatus`

In `cli/src/tui/model.rs`, alongside `current()` and `is_inflight()`:

```rust
/// The stale-refresh error message, if the last probe failed but a
/// previous successful pool snapshot is still on screen. None for every
/// other variant.
pub fn stale_error(&self) -> Option<&str> {
    match self {
        PoolStatus::ErrorStale(msg, _) => Some(msg.as_str()),
        _ => None,
    }
}
```

The doc comment justifies the accessor at the boundary per AGENTS.md.

### 2. Splice the banner into the global layout

In `cli/src/tui/view/mod.rs:1302-1360`, mirror the `alert_active` /
`advisory_height` pattern:

- Compute `let stale_msg = model.pool.stale_error();` once.
- Compute `let stale_height: u16 = if stale_msg.is_some() { 1 } else { 0 };`.
- Push `Constraint::Length(stale_height)` into `constraints` after the
  advisory constraint and before the tab bar constraint. Guard with
  `if stale_height > 0`.
- After the advisory render block, render the stale banner into
  `outer[off]` and bump `off += 1`. Banner spec:

```rust
let line = Line::from(Span::styled(
    format!(" pool data stale -- last pool refresh failed: {msg} "),
    Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD),
));
frame.render_widget(Paragraph::new(line), outer[off]);
off += 1;
```

Use `--` (double hyphen), not `—`, per the project CLI-output rule
(`AGENTS.md` "CLI Output Style").

The downstream slots stay at the same relative offsets (`outer[off]` =
tab bar, `outer[off + 1]` = spacer, `outer[off + 2]` = tab body,
`outer[off + 3]` = footer) -- the `off` counter already abstracts position
from the conditional banners. No changes needed at the `match model.tab`
dispatch or the footer render.

### 3. Snapshot test + substring + style assertions

Add to `mod tests` in `cli/src/tui/view/mod.rs` (alongside `snapshot_error`
at line 1588):

```rust
// Intent: A failed pool re-probe (PoolStatus::ErrorStale) renders a
//   yellow-bg "pool data stale -- last pool refresh failed: {msg}"
//   banner above the tab bar AND keeps the stale pool body visible
//   underneath, AND each non-space banner cell carries bg=Yellow,
//   fg=Black, BOLD.
// Why it exists: ErrorStale was introduced to preserve the last good
//   pool snapshot through a transient probe failure. Dropping the
//   message on the floor would leave the user unable to distinguish
//   stale data from fresh data -- the whole point of the discriminator.
//   The style assertion is necessary because buffer_to_string (and
//   insta snapshots in general -- see docs/tui-insta-guide.md) capture
//   symbols only, so a regression that emits the banner text in the
//   default style would slip past a text-only snapshot.
// Scenario: User pressed 'r'; btrfs spawn failed transiently; the
//   model is now ErrorStale("btrfs spawn failed: ENOENT", prev_pool).
#[test]
fn snapshot_stale_banner() {
    let model = Model::new_demo(
        sample_disk_names(),
        PoolStatus::ErrorStale(
            "btrfs spawn failed: ENOENT".to_owned(),
            sample_pool(),
        ),
    );
    let terminal = render(&model, 80, 24);
    let out = buffer_to_string(&terminal);

    // (1) Text content pinned explicitly. A future snapshot
    // regeneration cannot silently drop or alter the message.
    assert!(
        out.contains(
            "pool data stale -- last pool refresh failed: btrfs spawn failed: ENOENT"
        ),
        "stale banner text missing from rendered output:\n{out}"
    );

    // (2) Style pinned by direct buffer inspection. With no alert and
    // no advisories on the demo Model, the stale banner is row y=0.
    // Every non-space cell in that row must carry bg=Yellow, fg=Black,
    // BOLD.
    let buf = terminal.backend().buffer();
    let banner_y: u16 = 0;
    let mut checked = 0;
    for x in 0..buf.area.width {
        let cell = buf.cell((x, banner_y)).expect("cell in bounds");
        if cell.symbol() == " " {
            continue;
        }
        assert_eq!(cell.bg, Color::Yellow, "banner bg at x={x}");
        assert_eq!(cell.fg, Color::Black, "banner fg at x={x}");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "banner BOLD modifier at x={x}"
        );
        checked += 1;
    }
    assert!(checked > 0, "banner row had no non-space cells");

    // (3) Layout pinned by insta. Will produce a .snap.new on first
    // run -- review/accept per the cargo insta workflow.
    snap!(out);
}
```

The three assertions cover three orthogonal contracts:

- substring -- the error message reaches the user verbatim.
- per-cell style -- the banner is visually distinct (insta cannot
  enforce this; see `docs/tui-insta-guide.md:13`).
- snapshot -- the banner position relative to the rest of the layout
  doesn't silently shift (e.g. accidentally pushing the tab body).

Use width=80 to give the error string room without truncation. The
existing `snapshot_error` test uses 60x22; the wider canvas here is fine
because we're testing a specific banner, not the compact-mode layout.

`Color` and `Modifier` are already in scope at the top of the module
via the `Style::default().fg(...)` / `add_modifier(...)` call sites
(`cli/src/tui/view/mod.rs:97`, `:185`); if the test module's `use
super::*;` does not transitively reach them, add an explicit
`use ratatui::style::{Color, Modifier};` to `mod tests`.

### 4. Re-probe transition test (`cli/src/tui/app.rs`)

The view-level test above hand-constructs `PoolStatus::ErrorStale`,
so it does not exercise the production path that creates the state.
The real user sequence is two-step:

1. User presses `r` -> `Message::RefreshPool` (`cli/src/tui/app.rs:76`)
   reads `model.pool.current().cloned()` and transitions
   `Mounted(pool) -> Refreshing(pool)`, then queues `Effect::ProbePool`.
2. The probe completes -> `Message::PoolProbeFinished(Err(_), ...)`
   (`cli/src/tui/app.rs:141-170`) reads `current()` AGAIN on the now-
   `Refreshing(_)` state, finds the stale pool, and lands on
   `ErrorStale(err, stale)`.

The pool snapshot has to survive BOTH hops. A regression in either
hop -- `RefreshPool` accidentally flipping `Mounted -> Loading` (which
loses the `Some(stale)` arm at `app.rs:83-87`), or `PoolProbeFinished`
no longer reading `current()` before the match -- would make the
banner unreachable from a real `r` press while a test that only
dispatches `PoolProbeFinished(Err)` directly from `Mounted` still
passes.

Pin both hops in one test by driving the actual user sequence
(`RefreshPool` then `PoolProbeFinished(Err)`) and using a sentinel
field on `prev_pool` to prove the same instance survives both
transitions. Add this to the existing `mod tests` block in
`cli/src/tui/app.rs` (alongside `refresh_pool_sets_spinner_deadline`
at line 360, which already shows the `tempfile::tempdir()` +
`StatePaths::custom` setup pattern):

```rust
// Intent: The full manual-refresh sequence
//   (Mounted -> RefreshPool -> Refreshing -> PoolProbeFinished(Err))
//   must land on PoolStatus::ErrorStale, preserving both the error
//   string verbatim AND the same PoolState instance that was on
//   screen before the user pressed 'r'.
// Why it exists: The view layer renders the stale banner exclusively
//   on ErrorStale (cli/src/tui/view/mod.rs:986,1075). The state has
//   to survive two hops: RefreshPool's Mounted->Refreshing(stale)
//   read (cli/src/tui/app.rs:83-87), and PoolProbeFinished's read of
//   current() before the Err match (cli/src/tui/app.rs:142,167-170).
//   A regression in either hop -- RefreshPool flipping to Loading,
//   or PoolProbeFinished dropping the stale fallback -- would render
//   the banner unreachable from a real 'r' press. A sentinel field
//   on prev_pool that survives both hops proves the wiring is intact.
// Scenario: User has a mounted pool on screen, presses 'r', and the
//   subsequent btrfs spawn fails (e.g. ENOENT in a degraded PATH).
#[test]
fn refresh_then_probe_err_yields_error_stale_preserving_pool() {
    // Sentinel: a capacity_used_bytes value that sample_pool() will
    // not produce. Survives Clone trivially (u64) and is a single
    // primitive comparison, so the assertion is robust.
    const SENTINEL: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    let mut prev_pool = sample_pool();
    prev_pool.capacity_used_bytes = SENTINEL;

    let mut model = Model::new_demo(
        sample_disk_names(),
        PoolStatus::Mounted(prev_pool.clone()),
    );
    // RefreshPool early-returns unless model.paths.is_some()
    // (cli/src/tui/app.rs:77-79). Reuse the same setup as
    // refresh_pool_sets_spinner_deadline.
    let tmp = tempfile::tempdir().unwrap();
    model.paths = Some(crate::state_paths::StatePaths::custom(tmp.path().into()));

    // Hop 1: Mounted -> Refreshing(stale). Assert the sentinel
    // survived RefreshPool's read of current().
    update(&mut model, Message::RefreshPool);
    match &model.pool {
        PoolStatus::Refreshing(p) => assert_eq!(
            p.capacity_used_bytes, SENTINEL,
            "RefreshPool dropped the stale pool (Mounted -> Refreshing)"
        ),
        other => panic!(
            "after RefreshPool: expected Refreshing(_), got discriminant {:?}",
            std::mem::discriminant(other)
        ),
    }

    // Hop 2: Refreshing -> ErrorStale. Assert the sentinel ALSO
    // survived PoolProbeFinished's read of current() before the
    // Err match, and that the error string is verbatim.
    update(
        &mut model,
        Message::PoolProbeFinished(
            Err("btrfs spawn failed: ENOENT".to_owned()),
            Duration::from_millis(50),
        ),
    );
    match &model.pool {
        PoolStatus::ErrorStale(msg, kept) => {
            assert_eq!(msg, "btrfs spawn failed: ENOENT");
            assert_eq!(
                kept.capacity_used_bytes, SENTINEL,
                "PoolProbeFinished(Err) dropped the stale pool",
            );
        }
        other => panic!(
            "after PoolProbeFinished(Err): expected ErrorStale, got discriminant {:?}",
            std::mem::discriminant(other)
        ),
    }

    // Keep the tempdir alive until end of test so StatePaths is valid.
    drop(tmp);
}
```

Notes for the implementer:

- `Duration` is already imported in `app.rs` (used at line 82); reuse
  the same path. `PoolState` derives `Clone` only
  (`cli/src/tui/model.rs:187`), so `prev_pool.clone()` works; do NOT
  attempt `assert_eq!(kept, &prev_pool)` -- there is no `PartialEq`
  on `PoolState` and adding derives is out of scope.
- The `tempfile` crate is already a dev-dependency (see
  `refresh_pool_sets_spinner_deadline` at line 362). No new dep needed.
- The two `match` arms intentionally use `std::mem::discriminant(...)`
  in the panic message because `PoolStatus` does not derive `Debug`
  (any of the contained `PoolState` fields without `Debug` would block
  it). The discriminant is enough to tell the reader which arm was hit.
- Do not assert on `model.probe_duration` here -- that's covered
  implicitly by other `PoolProbeFinished` tests and is irrelevant to
  the banner contract.

## Verification

End-to-end checks, in order:

1. `just test-rust` -- runs `cargo test`. Two new tests run:
   `snapshot_stale_banner` in `cli/src/tui/view/mod.rs` and
   `refresh_then_probe_err_yields_error_stale_preserving_pool` in
   `cli/src/tui/app.rs`.

   The app-level test must pass green on this run -- it has no snapshot
   dependency and is pure state-transition assertions.

   The view-level test is expected to **fail on this first run**: insta
   has no committed `.snap` yet and writes a pending
   `cli/src/tui/view/snapshots/snapshot_stale_banner.snap.new` instead
   (per `docs/tui-insta-guide.md:19,41`). The non-snapshot assertions
   (substring + per-cell style) must still pass on this run -- if any
   of those fail, the implementation is wrong and the snapshot is not
   safe to accept.

   Filename note: the `snap!` macro at
   `cli/src/tui/view/mod.rs:1424-1430` sets
   `prepend_module_to_snapshot => false`, so the snapshot lands at
   `snapshot_stale_banner.snap` (matching the existing
   `snapshot_error.snap` convention) -- not the
   `tui__view__tests__*` prefix that insta uses by default.

2. `cargo insta review` (or `cargo insta accept` if the diff against
   the pending `.snap.new` looks right). This promotes the `.snap.new`
   to `.snap`.

3. Re-run `just test-rust`. With the accepted snapshot in place, the
   new test must now pass green. Confirm no other snapshot in
   `cli/src/tui/view/snapshots/` regressed (global layout slot count
   only changes when `ErrorStale` is active, so every other variant
   should be byte-identical).

4. `cargo check -p braid-cli` -- confirms `stale_error` is wired up and
   not flagged dead.

5. Manual TUI sanity (optional, only if Linux + a NixOS VM is handy):
   in a dev VM with a mounted pool, temporarily inject a probe error
   via the existing test harness or by stopping `btrfs` mid-probe, then
   press `r` and confirm the yellow banner appears above the tab bar
   with the error text and the stale pool body remains visible. Skip
   this if no VM is convenient -- the snapshot + style assertions pin
   the visible contract.

## Out of scope

- Changing `is_inflight()` to include `ErrorStale`. The spinner is
  meant to indicate a probe is currently running; ErrorStale means the
  probe finished (with an error). The banner is the right signal, not
  a phantom spinner.
- Appending the error to the footer's "Reload: r ({}ms)" string. The
  footer is one line wide and would truncate non-trivial errors; the
  banner gives the message room.
- Modifying `view_data` / `view_scrub` match arms. They already render
  the stale pool body correctly; the banner replaces the need to add a
  per-section indicator.
- Auto-retrying the probe or any production change to the
  `cli/src/tui/app.rs` update loop. Scope here is purely the visible
  signal for the existing state machine -- the new app.rs unit test in
  step 4 only pins the existing transition, it does not change it.
