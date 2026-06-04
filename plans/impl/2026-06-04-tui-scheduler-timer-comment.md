# Plan: ideal comment for the TUI sleep-in-worker schedulers

## Context

A review finding (Low / Simplicity) objected that `Effect::ScheduleFanProbe`
and `Effect::ScheduleUpsProbe` spawn a full panic-guarded `spawn_worker` whose
entire body is `thread::sleep(delay)` -- "a thread just to sleep" -- and
proposed routing the delay through "the existing mpsc/main-loop timeout
machinery (or a shared timer thread)."

Verification (see the `/verify-issue` analysis) concluded the proposed refactor
should **not** be done:

- The "existing main-loop timeout machinery" does not exist as a scheduler.
  `run_loop`'s `recv_timeout(timeout)` (`cli/src/tui/mod.rs#run_loop`) is a
  *redraw* cadence only (`FRAME_BUDGET` animating / `IDLE_REDRAW_INTERVAL`
  idle). Implementing the fix means *building* deadline-folding into the hottest
  shared function, or adding a generic timer-effect abstraction -- net-new
  machinery, not reuse, and it would couple probe cadence to redraw cadence.
- Routing schedulers through `spawn_worker` was deliberate and documented
  (commit `994d7389`): one panic-safe boundary so no spawn can strand a
  subsystem, plus the `IN_CAUGHT_WORKER` terminal-safety marking. The per-tick
  spawn cost is negligible (two 5s cadences).

So the code is correct and intentional. The real gap is documentation: a
reviewer objected **despite** the existing comments, because those comments
explain *how* (routed through `spawn_worker`, fallback re-emits the tick) but
never *why a sleep-in-worker instead of a timer*. The intended outcome of this
change is to close that gap with one tight comment so this class of finding does
not recur -- and explicitly without altering any behavior.

## Scope

Comment-only edit to `cli/src/tui/effect.rs`. No behavior change, no signature
change, no new/removed code. The two scheduler arms are the only `thread::sleep`
timers in the TUI (`rg "thread::sleep" cli/src/tui` -> the two arms only), and
there is no TUI-architecture ADR or doc page, so the code comment is the single
authoritative home for this rationale -- no duplicate-authority risk.

## Change

Make the **`ScheduleFanProbe` arm the canonical home** for the rationale (the
`ScheduleUpsProbe` arm already points at it with "see ScheduleFanProbe"). Move
the rationale to a block comment at the **top of the arm** so the "why" precedes
the mechanism, and leave the `on_panic` fallback bare (the block already
explains it). This reads better than the current placement, where the rationale
hangs on the fallback closure.

### `Effect::ScheduleFanProbe` (currently `cli/src/tui/effect.rs#execute_effect`, ~lines 114-126)

Replace the arm with:

```rust
Effect::ScheduleFanProbe { delay } => {
    // Sleep-in-worker timer, deliberately not a main-loop deadline: with
    // only two 5s cadences the per-tick spawn is negligible, and routing
    // through spawn_worker keeps every execute_effect spawn behind one
    // panic-safe boundary. (A loop-side deadline would couple probe cadence
    // to the redraw timeout.) The sleep can't panic; the on-panic fallback
    // re-emits the tick only to honor that single-boundary invariant.
    spawn_worker(
        cmd_tx,
        move || {
            thread::sleep(delay);
            Event::PollFanRefresh
        },
        |_| Event::PollFanRefresh,
    );
}
```

### `Effect::ScheduleUpsProbe` (currently `cli/src/tui/effect.rs#execute_effect`, ~lines 139-150)

Keep the pointer to the canonical comment and **keep the distinct-ticks
invariant warning** (it is a real correctness note, not timer rationale):

```rust
Effect::ScheduleUpsProbe { delay } => {
    // Sleep-in-worker like ScheduleFanProbe. PollUpsRefresh and
    // PollFanRefresh are distinct ticks -- do not collapse them.
    spawn_worker(
        cmd_tx,
        move || {
            thread::sleep(delay);
            Event::PollUpsRefresh
        },
        |_| Event::PollUpsRefresh,
    );
}
```

### What the wording deliberately covers

1. **Why not a timer / main-loop deadline** (the gap that produced the finding):
   negligible per-tick cost for two 5s cadences; a loop deadline would couple
   probe cadence to the redraw timeout.
2. **Why `spawn_worker`** (the documented uniformity invariant): every
   `execute_effect` spawn sits behind one panic-safe boundary.
3. **Why a fallback for a sleep that can't panic**: it exists only to honor the
   single-boundary rule, not as live defense.

Do not touch `spawn_worker`'s doc comment
(`cli/src/tui/effect.rs#spawn_worker`): it documents the shared boundary for all
six workers and is accurate; the timer-vs-deadline rationale is specific to the
two scheduler arms and belongs there, not on the shared helper.

## Style conformance

- ASCII only; `--` not em-dash (matches the file and AGENTS.md CLI/Doc rules).
- References other code by symbol (`spawn_worker`, `ScheduleFanProbe`), never by
  line number, per AGENTS.md File References.
- ~6 lines on the canonical arm: longer than the 1-3 line default, justified
  because it records a deliberate design decision that has already been
  questioned once.

## Verification

Comment-only, so the meaningful checks are that the arms still compile and the
existing worker tests still pass (no behavior change):

1. `cargo build -p braid-cli` (or `just test-rust`) -- confirms the comment did
   not break the arm structure.
2. `just test-rust` -- the two `effect.rs` unit tests
   (`panicking_worker_delivers_fallback_event_with_message`,
   `non_panicking_worker_delivers_body_event`) and the `app.rs` scheduler tests
   still pass unchanged.
3. Visual: `git diff cli/src/tui/effect.rs` shows only comment lines added/moved
   and (for the Fan arm) the rationale relocated above `spawn_worker(`; no
   changes to `move ||` bodies or fallback expressions.

No VM tests needed -- this does not touch systemd, the module, or any parser.
