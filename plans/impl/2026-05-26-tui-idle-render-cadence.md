# Plan: stop the idle 60Hz busy-redraw in the TUI render loop

## Context

`run_loop` in `cli/src/tui/mod.rs` redraws every iteration and then waits on
`rx.recv_timeout(FRAME_BUDGET=16ms)`. The 16ms timeout yields no message, so
`update()` only ever runs on a real channel event -- but the loop still wakes
~60x/second to increment `model.frame`, recompute `now`, and re-run `view()`
even when nothing is happening. On an always-on NAS during a long-lived
`braid tui` SSH session this is continuous wasted work.

`model.frame` is consumed *only* by the three spinner-glyph sites
(`view/mod.rs:1527`, `browse/view.rs:22`, `browse/view.rs:118`, all
`SPINNER[(frame/8) % len]`), and the per-frame `now` is consumed *only* by
`view_scrub`'s `timeago()`, whose output is minute-granular. So sub-second
wakeups are needed *only* while a spinner is animating; the rest of the time a
slow tick is enough to keep the minute-granular timeago fresh.

Most wake reasons are already channel messages: key input (the 100ms-poll input
thread, `event.rs:69-83`), probe completions (worker threads), and the fan/UPS
auto-poll cadence (`ScheduleFanProbe`/`ScheduleUpsProbe` spawn
`thread::sleep`-then-send timer threads, `effect.rs:95-116`). There is one
exception: terminal **resize**. The input thread reads crossterm events but only
forwards `Event::Key` (`event.rs:73`) -- `Resize` is read and dropped. Today that
is invisible because the 60Hz redraw re-queries the terminal size every frame;
once we stop the busy-redraw, a resize would leave the UI stale until the next
idle tick. So this plan also makes resize a channel wake (section 4). Nothing
else depends on the 16ms render tick itself.

**Outcome:** the loop ticks at 16ms only while a spinner glyph must advance, and
otherwise wakes every 10s (so idle timeago/data still refreshes without the user
pressing a key). The animation gate is a tested `Model::is_animating()` predicate
built on a shared `pool_spinner_active()` that the footer also calls, so the loop
and the footer spinner cannot disagree about the pool spinner. No probes fire
from the render tick either way, so disk-spindown posture (decisions 015/016) is
unaffected.

This is the pivot from the reviewed finding: same mechanism, but the gate lives
in a tested `Model::is_animating()` instead of an inline boolean in the
untestable loop, and it dedups the `is_inflight() || spinner_deadline-live`
expression already living in the footer view.

## Approach

### 1. Two predicates on `Model` (`cli/src/tui/model.rs`)

Add to `impl Model` (alongside `new`/`new_demo`); both need `///` doc comments
per the project's doc-comment rule. `Instant` is already imported (`model.rs:2`).

```rust
/// True while the pool "Reload: r" spinner should be visible: a probe is in
/// flight, or we're still inside the minimum-visible window after a refresh.
/// Single source of truth shared by the footer view and the render-loop
/// animation gate so the two cannot disagree about when the glyph advances.
pub fn pool_spinner_active(&self) -> bool {
    self.pool.is_inflight() || self.spinner_deadline.is_some_and(|d| Instant::now() < d)
}

/// True while any spinner glyph needs sub-second advancement. Drives the
/// render loop's fast (16ms) vs idle (10s) wait cadence; eliminates the
/// idle 60Hz busy-redraw on an always-on NAS.
pub fn is_animating(&self) -> bool {
    self.pool_spinner_active() || (self.tab == Tab::Browse && self.browse.loading())
}
```

`Tab` is defined in `model.rs` and derives `PartialEq`/`Eq` (`model.rs:12`), so
the `self.tab == Tab::Browse` comparison needs no new import or derive.

The Browse half **must** be gated on `self.tab == Tab::Browse`. Browse spinners
render only inside `view_browse`, which the view calls solely for `Tab::Browse`
(`view/mod.rs:1519`) -- the command-line spinner at `browse/view.rs:21` and the
content "loading..." at `browse/view.rs:117` (the latter additionally gated on
empty output, a strict subset). But `browse.loading()` (`browse/state.rs:817`)
can stay `true` after the user tabs away: a Browse command dispatched via
`load_current` keeps `loading == true` until `command_finished` fires, and
`NextTab`/`PrevTab` only reload when the *target* tab is Browse
(`browse_load_if_active`, `app.rs:330`). Without the tab-gate, a slow background
Browse command would hold the loop at the 16ms cadence with no visible spinner --
exactly the idle busy-redraw this plan removes. With `tab == Tab::Browse`, the
fast cadence engages only when a Browse spinner is actually on screen (on that
tab, `loading()` always shows at least the command-line glyph).

### 2. Variable-cadence wait in `run_loop` (`cli/src/tui/mod.rs`)

Add a constant next to `FRAME_BUDGET` (`mod.rs:67`):

```rust
/// Idle redraw cadence when no spinner is animating: slow enough to be
/// effectively free on an idle NAS, fast enough that minute-granular
/// timeago and any out-of-band state stay current without user input.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(10);
```

Restructure the loop body so the cadence is decided from the state we are
*about to render*, then replace the `if let Ok(event) = rx.recv_timeout(...)`
block (`mod.rs:92-105`) with an explicit match. Sample `is_animating()` **before**
`terminal.draw`, not after: the footer re-checks `pool_spinner_active()` inside
the draw closure, so a `spinner_deadline` that expires in the window between draw
and a post-draw check would strand a just-drawn spinner on screen for a whole
idle interval (up to 10s). Sampling before draw makes the fast wait conservative
-- whenever a frame *could* render a spinner, `animating` was true, so a 16ms
wait follows. Keep the `model.frame` increment, the `now` block, the `try_recv`
drain (up to `MAX_EVENTS_PER_FRAME`), and the update/effect handling unchanged:

```rust
model.frame = model.frame.wrapping_add(1);
let now = { /* unchanged naive-local PrimitiveDateTime */ };

// Sample BEFORE draw -- see note above.
let animating = model.is_animating();
terminal.draw(|f| view(model, f, now))?;

let timeout = if animating {
    FRAME_BUDGET
} else {
    IDLE_REDRAW_INTERVAL
};

let mut messages = Vec::new();
match rx.recv_timeout(timeout) {
    Ok(event) => {
        let ctx = key_context(model);
        messages.extend(event.into_message(&ctx));
        for _ in 1..MAX_EVENTS_PER_FRAME {
            match rx.try_recv() {
                Ok(event) => {
                    let ctx = key_context(model);
                    messages.extend(event.into_message(&ctx));
                }
                Err(_) => break,
            }
        }
    }
    Err(mpsc::RecvTimeoutError::Timeout) => {}
    Err(mpsc::RecvTimeoutError::Disconnected) => break,
}
```

Notes:
- `Timeout` => empty `messages`; the next loop redraws (advances the spinner at
  16ms, or refreshes timeago at the 10s idle tick). Same as today.
- `Disconnected => break` is a minor robustness improvement folded into the
  restructure: today's `if let Ok` treats Disconnected like Timeout and would
  busy-spin at 100% CPU (Disconnected returns immediately). It cannot occur
  while `run_loop` holds `cmd_tx`, but breaking is the correct defensive handling
  and removes the latent spin. `mpsc` is already imported (`mod.rs:13`).
- A received `Event::Resize` (section 4) maps to no `Message`, so it falls
  through this match as an empty `messages` batch; the wake alone drives the
  top-of-loop redraw, and ratatui's `terminal.draw` autoresize picks up the new
  size. No `now`/timeout special-casing needed.
- Leave `model.frame` / `now` computed at the top every iteration. While idle
  they advance once per 10s tick -- invisible (no spinner shown) and exactly
  what refreshes the timeago.

### 3. Dedup the footer (`cli/src/tui/view/mod.rs:1522-1523`)

Replace:

```rust
let spinning =
    model.pool.is_inflight() || model.spinner_deadline.is_some_and(|d| Instant::now() < d);
```

with:

```rust
let spinning = model.pool_spinner_active();
```

Identical behavior, so the existing footer snapshot tests
(`snapshot_footer_spinner_inflight`, `snapshot_footer_duration_after_spinner`)
stay green with no churn. This is the only other site computing that
expression (confirmed).

### 4. Make terminal resize a channel wake (`cli/src/tui/event.rs`)

The input thread currently forwards only key events; `Resize` is read and
dropped (`event.rs:73`). At the 60Hz busy-redraw this is masked, but once the
loop idles at 10s a resize would leave the UI stale. Add a wake-only event:

- Add a payload-free `Resize` variant to the `Event` enum (`event.rs:17-31`). No
  dimensions needed -- `terminal.draw` autoresizes by re-querying the backend
  size each draw, so the wake alone reflows.
- In `Event::into_message` (`event.rs:34-53`), add `Event::Resize => None`: it is
  a pure wake; the redraw happens at the top of the next loop iteration.
- Factor the crossterm -> TUI mapping into a small **pure** helper so the
  forwarding is unit-testable without a live terminal (the input thread's
  `event::read()` cannot be driven from a test, so an accidentally dropped
  `Resize` arm is otherwise invisible to `just test-rust`). `event::Event` is
  crossterm's type (the `event` module alias already in scope, `event.rs:8`); the
  bare `Event` is this module's enum:

```rust
/// Map a crossterm terminal event to the TUI's internal event, or `None` for
/// events the TUI ignores. Pure and total so the input thread's key+resize
/// forwarding is unit-testable -- a dropped `Resize` arm would otherwise pass
/// `just test-rust` and only surface as a stale UI after the idle change.
fn to_tui_event(ev: event::Event) -> Option<Event> {
    match ev {
        event::Event::Key(key) => Some(Event::Key(key)),
        event::Event::Resize(_, _) => Some(Event::Resize),
        _ => None, // mouse/focus/paste: not used by this TUI
    }
}
```

- In the input thread (`event.rs:69-83`), replace the
  `if let Ok(event::Event::Key(key)) = event::read()` let-chain with a call
  through the helper, preserving the existing send-error / read-error -> `break`
  shutdown (let-chain matches the file's existing idiom):

```rust
match event::read() {
    Ok(ev) => {
        if let Some(event) = to_tui_event(ev)
            && thread_tx.send(event).is_err()
        {
            break;
        }
    }
    Err(_) => break, // read failure: tear the input thread down
}
```

## Reused existing code

- `PoolStatus::is_inflight()` (`model.rs:314`) -- unchanged.
- `Model.spinner_deadline` (`model.rs:330`) -- unchanged; still set by
  `RefreshPool` (`app.rs:100`) and `Model::new` (`model.rs:394`).
- `BrowseState::loading()` (`browse/state.rs:817`) -- unchanged.
- Timer/worker architecture (`effect.rs`) -- unchanged; it already delivers
  probe completions and the fan/UPS poll cadence as messages.
- `event.rs` -- gains the `Resize` wake variant, the pure `to_tui_event`
  mapping helper, and its input-thread call (section 4); the channel wiring and
  shutdown path are otherwise unchanged.

## Tests

Add to the existing `#[cfg(test)] mod tests` in `cli/src/tui/model.rs` (which
already imports `update`, `Message`, `Duration`, `Instant`, and demo helpers).
Test `is_animating()` directly -- behavioral and structure-insensitive (they
pass whether or not `pool_spinner_active` is factored out):

- `is_animating_false_when_idle`: `new_demo(Mounted, ..)` (spinner_deadline is
  `None` in demo), Data tab, no browse load -> `!is_animating()`.
- `is_animating_true_when_pool_inflight`: `new_demo(.., PoolStatus::Loading)` ->
  `is_animating()`.
- `is_animating_true_when_spinner_deadline_live`: demo Mounted,
  `spinner_deadline = Some(Instant::now() + Duration::from_secs(10))` ->
  `is_animating()`.
- `is_animating_false_when_spinner_deadline_expired`: demo Mounted,
  `spinner_deadline = Some(Instant::now() - Duration::from_secs(1))` ->
  `!is_animating()`. (Pins the expiry edge the loop relies on to drop back to
  the idle cadence.)
- `is_animating_true_when_browse_loading`: demo Mounted, drive into Browse via
  `update(&mut model, Message::NextTab)` x2 (Data->Scrub->Browse) -- this runs
  `browse_load_if_active` -> `load_current`, which sets `loading = true` and
  returns an unexecuted `BrowseRunCommand`, so `browse.loading()` stays true ->
  assert `model.browse.loading() && model.is_animating()` (still on the Browse
  tab, so the gate passes). Mirrors the existing `next_tab_into_browse_emits_effect`
  test.
- `is_animating_false_when_browse_loading_off_tab` (regression for the tab-gate,
  finding 1): continue from the setup above, then `update(&mut model,
  Message::NextTab)` once more (Browse->Data). `browse_load_if_active` no-ops off
  Browse, so `browse.loading()` stays `true`, but `tab` is now `Data` -> assert
  `model.browse.loading() && !model.is_animating()`. Without the
  `tab == Tab::Browse` gate this would be `true` and hold the loop at the 16ms
  cadence with no spinner on screen.

In `cli/src/tui/event.rs`'s existing `#[cfg(test)] mod tests` (which already has
the `ctx()` helper and `into_message` tests), pin the resize wake path so a
future edit can't silently drop `Resize` forwarding:

- `to_tui_event_forwards_resize_and_keys`: assert `to_tui_event(event::Event::
  Resize(80, 24))` is `Some(Event::Resize)`, `to_tui_event(event::Event::Key(..))`
  is `Some(Event::Key(_))`, and an ignored crossterm variant (e.g.
  `event::Event::FocusGained`) is `None`. Assert with `matches!(...)`, not `==`:
  `Event` has no `PartialEq` derive (`event.rs:16-17`; payloads include
  `PoolState`/`FanSnapshot`). Refer to crossterm's type as `event::Event` to
  avoid clashing with the local `Event` enum.
- `resize_into_message_is_none`: `Event::Resize.into_message(&ctx())` is `None` --
  a pure wake produces no `Message`.

No `run_loop` test: it owns the terminal and blocks on a channel, and the risk
now lives in `is_animating()` and `to_tui_event` (both tested) plus a trivial
`if`-select of the timeout. The footer's use of `pool_spinner_active()` stays
covered by the existing snapshot tests.

## Verification

1. `just test-rust` -- runs the new `is_animating` and `to_tui_event`/resize-wake
   unit tests plus the existing model/app/view (footer snapshot) tests. This is
   the primary gate.
2. Manual smoke (optional, on a NixOS host/VM with a TTY): `braid tui` and
   confirm:
   - Idle (Data tab, no refresh): no continuous redraw; CPU drops to ~idle.
   - Press `r`: the "Reload: r" braille spinner animates smoothly for the full
     ~500ms minimum, then shows `(Nms)` -- i.e. fast cadence engages while
     `pool_spinner_active()` is true and stops cleanly when it expires.
   - Browse tab while a command loads: the `|/-\` spinner animates.
   - On a fan/UPS-configured host, the footer/data still updates on the 5s poll;
     on a host with neither, idle data still refreshes within ~10s without input.
   - Resize the terminal while idle (Data tab, no refresh): the UI reflows
     promptly via the resize wake -- it does not wait for the 10s idle tick.
3. No fixture/parser impact (no tool-output parsing touched), so no
   `capture-*-fixtures` run is required.

## Out of scope / non-goals

- No change to probe cadence or what the render tick triggers (it triggers no
  probes), so decisions 015/016 (HDD spindown / auto-suspend) are untouched.
- No change to `spinner_deadline` semantics, the 500ms minimum-visible window,
  or the fan/UPS scheduler.
- Not switching the spinner off `model.frame` onto a wall-clock tick -- the
  frame counter stays the animation source; only the loop's wait cadence changes.
