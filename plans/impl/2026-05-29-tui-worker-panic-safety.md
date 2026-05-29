# Plan: panic-safe TUI effect workers + gated ratatui panic hook

## Context

The braid TUI runs each background probe on a detached worker thread spawned in
`cli/src/tui/effect.rs::execute_effect`. Every worker does its work and then
sends exactly one terminal `Event` back over an mpsc channel
(`PoolProbeFinished`, `FanProbeFinished`, `UpsProbeFinished`,
`BrowseCommandFinished`). The `tx.send(...)` is the **last** statement in the
closure, so if the worker body panics, the send is skipped and that subsystem
is stranded:

- **Pool:** `model.pool` never leaves `Loading`/`Refreshing` -- the pool view
  spins forever. The run loop uses `recv_timeout` (not a blocking `recv`), so
  the process does not deadlock, but that view is permanently stuck.
- **Fan / UPS:** the next 5s probe is rearmed **only inside** the
  `*ProbeFinished` handler (app.rs `FanProbeFinished`/`UpsProbeFinished` arms).
  A panicking worker means `*_probe_inflight` stays `true` forever and the
  reschedule never fires -- the cadence is permanently dead.

Today the most visible panic sources are four `.expect()` calls in
`cli/src/tui/probe.rs` (lines 116, 268, 344, 346) that re-parse
membership-derived `DiskName`/`ByIdPath` strings. These are **not reachable in
practice** -- the strings come from already-validated typed values
(`DiskMember.name: DiskName`, `by_id: ByIdPath`), `from_membership` stringifies
them, and `DiskName::parse`/`ByIdPath::parse` are idempotent on their own
`as_str()` (they store the raw string unchanged with no normalization), and
membership loads through validating `Deserialize`. So this is a robustness /
defense-in-depth fix, not a live bug.

A second gap turns the `catch_unwind` fix below into a half-measure on its own.
`ratatui::init()` installs a global panic hook (verified against ratatui 0.30.0
`init.rs:499-505`: `set_panic_hook` saves the current hook, then sets one that
runs `restore()` -- disable raw mode, leave the alternate screen -- **then** the
saved hook). That hook fires on **every** thread, during the panic, before our
`catch_unwind` catches it. So a *caught* worker panic would still trip ratatui's
`restore()` and **tear down the live TUI mid-session** (alternate screen left,
raw mode disabled) while `run_loop` keeps drawing -- far worse than a stray
stderr line. Catching the unwind stops the hang but not the teardown. (A
main-thread panic, by contrast, is *already* handled correctly by ratatui's
hook: restore then print. That case needs no change.)

So the worker `catch_unwind` and a hook adjustment are coupled: we must stop
ratatui's `restore()` from firing for the panics we intend to catch, while
leaving it intact for every thread we do not catch (main thread, input reader).

**Outcome:** a panic in any probe worker becomes that subsystem's normal
"finished with error / unknown" event (rendered cleanly, loop stays alive)
instead of a permanent hang or a torn-down terminal; threads we do not catch
keep ratatui's restore-then-print behavior unchanged. The fix is one shared
helper applied across all workers plus a small hook adjustment -- not four local
patches.

## Approach

The model/app layer already handles every terminal event correctly
(`PoolProbeFinished(Err(_))` -> `Error`/`ErrorStale`, tested at
app.rs:639-680; fan/ups finished arms clear the inflight flag and rearm the
scheduler; `BrowseCommandFinished` clears `browse.loading` on a generation
match). So **no model/app changes are needed** -- the fallback only needs to
emit a valid terminal event of the same variant. This keeps the blast radius to
three files.

### 1. Shared `spawn_worker` helper (effect.rs)

Replace the raw `thread::spawn` boilerplate at all spawn sites in
`execute_effect` with one helper that runs the body under `catch_unwind` and
guarantees a terminal event is sent even on panic:

```rust
use std::any::Any;
use std::cell::Cell;

thread_local! {
    /// Set on threads spawned by `spawn_worker`. The TUI panic hook reads this
    /// (via `in_caught_worker`) to decide whether to run ratatui's
    /// restore-then-print: false (main / input threads) -> restore + print;
    /// true (caught worker) -> do nothing, because `catch_unwind` here will
    /// recover and the live TUI must survive.
    static IN_CAUGHT_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is a `spawn_worker` thread whose panic will be
/// caught. The panic hook in `mod.rs` gates ratatui's `restore()` on this, so
/// the `Cell` stays encapsulated here rather than being read across the module
/// boundary.
pub(crate) fn in_caught_worker() -> bool {
    IN_CAUGHT_WORKER.with(Cell::get)
}

/// Extract a human-readable message from a caught panic payload so a worker
/// panic surfaces in the rendered error instead of being lost.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Spawn a TUI worker thread that always delivers exactly one terminal `Event`,
/// even if `body` panics. The strand-prevention invariant lives here so every
/// probe/scheduler worker shares one panic-safe boundary instead of trusting
/// each closure to reach its `tx.send`. Marks the thread as caught so the panic
/// hook leaves the live terminal alone (see Section 3).
fn spawn_worker<B, P>(cmd_tx: &mpsc::Sender<Event>, body: B, on_panic: P)
where
    B: FnOnce() -> Event + Send + 'static,
    P: FnOnce(String) -> Event + Send + 'static,
{
    let tx = cmd_tx.clone();
    thread::spawn(move || {
        IN_CAUGHT_WORKER.with(|f| f.set(true));
        let event = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(ev) => ev,
            Err(payload) => on_panic(panic_message(&*payload)),
        };
        let _ = tx.send(event);
    });
}
```

Rewrite each spawn site to pass a `body` (the existing work, returning the
success `Event`) and an `on_panic` (builds the fallback). `AssertUnwindSafe` is
sound here: all captures are plain data and the thread is discarded after the
send, so nothing observes a post-unwind broken invariant.

Per-site fallbacks:

| Worker | `on_panic` fallback |
| --- | --- |
| `ProbePool` | `\|msg\| Event::PoolProbeFinished(Err(format!("pool probe panicked: {msg}")), Duration::ZERO)` (elapsed measured inside `body`; `Duration::ZERO` on panic is fine -- it only feeds the footer's probe-duration display) |
| `ProbeFan` | `\|_\| Event::FanProbeFinished(FanSnapshot::unknown())` |
| `ProbeUps` | `\|_\| Event::UpsProbeFinished(UpsSnapshot::unknown())` |
| `BrowseRunCommand` | `move \|msg\| Event::BrowseCommandFinished { raw: RawCommandOutput { cmd: String::new(), stdout: String::new(), stderr: format!("browse command panicked: {msg}"), exit_status: 1 }, generation }` (`generation: u64` is `Copy`, captured in both closures) |
| `ScheduleFanProbe` | body `\|\| { thread::sleep(delay); Event::PollFanRefresh }`; on_panic `\|_\| Event::PollFanRefresh` |
| `ScheduleUpsProbe` | body `\|\| { thread::sleep(delay); Event::PollUpsRefresh }`; on_panic `\|_\| Event::PollUpsRefresh` |

The two schedulers emit **distinct** poll events (`event.rs`: `PollFanRefresh`
-> `RefreshFan`, `PollUpsRefresh` -> `RefreshUps`); do not collapse them. Sleep
cannot realistically panic -- they are routed through `spawn_worker` only so no
spawn in `execute_effect` is left unguarded; mark each `on_panic` as
defensive/unreachable in a comment.

Note the asymmetry to document in a comment: pool and browse fallbacks carry the
panic message; fan/ups fallbacks reset to an `Unknown` snapshot with **no**
message (identical to a normal failed probe). Acceptable -- those probes are
low-risk and the degraded render is honest.

### 2. `unknown()` snapshot constructors (model.rs)

Add small constructors next to the structs so the panic path builds a degraded
snapshot **without invoking a runner** (the panic path must not re-enter command
execution):

```rust
impl FanSnapshot {
    /// Degraded snapshot for the panic-fallback path: renders identically to
    /// the pre-probe `None` state without re-invoking a runner.
    pub fn unknown() -> Self {
        FanSnapshot { fan: None, driving: None, daemon: DaemonStatus::Unknown, probed_at: Instant::now() }
    }
}

impl UpsSnapshot {
    /// Degraded snapshot for the panic-fallback path; see `FanSnapshot::unknown`.
    pub fn unknown() -> Self {
        UpsSnapshot {
            flags: Vec::new(),
            battery_charge_pct: None,
            runtime_secs: None,
            load_pct: None,
            watts_estimated: None,
            raw_text: String::new(),
            daemon: DaemonStatus::Unknown,
            probed_at: Instant::now(),
        }
    }
}
```

Do **not** reuse `probe.rs::ups_snapshot_query_failed` -- it takes a runner and
calls `probe_daemon_status` (spawns a command), which is exactly what the panic
path must avoid. `daemon: Unknown` is required: the view's fan/ups sections read
these fields with `.map(...).unwrap_or(Unknown)` (verified -- no `.expect()` on
snapshot fields), so an all-`None`/`Unknown` snapshot renders like the
pre-probe state.

### 3. Gate ratatui's panic hook by caught-worker status (mod.rs `run_with_model`)

`ratatui::init()` already installs the hook we need for ordinary panics
(`restore()` then the prior hook). The only change required is to stop that
`restore()` from firing for the worker panics we now catch -- otherwise a caught
probe panic tears down the live TUI. So we take ratatui's hook and wrap it in a
gate keyed on the `IN_CAUGHT_WORKER` thread-local from Section 1:

```rust
let mut terminal = ratatui::init();          // installs ratatui's restore-then-print hook
let ratatui_hook = std::panic::take_hook();  // take it so we can gate it (must be AFTER init)
std::panic::set_hook(Box::new(move |info| {
    // Caught worker panics (spawn_worker set the flag) must NOT run ratatui's
    // restore() -- catch_unwind there recovers and the live TUI must survive.
    // Every other thread (main loop, input reader) keeps ratatui's exact
    // restore-then-print behavior.
    if !effect::in_caught_worker() {
        ratatui_hook(info);
    }
}));

let (_input, cmd_tx, rx) = InputHandler::new();
// ... run init_effects, run_loop ...
let result = run_loop(...);
ratatui::restore();
let _ = std::panic::take_hook(); // drop our gating hook; std reinstalls its default
result
```

Why gate on the thread-local rather than a main-thread id compare: the flag
marks **exactly** the threads that have `catch_unwind` recovery. A
`current().id() == main_id` test would also suppress `restore()` for the input
reader thread (`event.rs:82`), so a panic there would leave the terminal in raw
mode (no `Ctrl-C`) -- strictly worse than stock ratatui. Gating on
`IN_CAUGHT_WORKER` leaves the input thread (and any future un-wrapped thread)
with ratatui's normal restore-then-print, regressing nothing.

Mechanics (verified against ratatui 0.30.0 `init.rs`):
- `restore()` (init.rs:457-461) is panic-safe -- it `eprintln`s on `try_restore`
  failure rather than panicking -- so calling `ratatui_hook(info)` cannot cause
  a double-panic abort. No extra guard needed.
- On a **main-thread** panic: the gate is `false`, `ratatui_hook` runs
  `restore()` + print once; unwinding then bypasses the line-63 `restore()`, so
  the terminal is restored exactly once.
- On a **caught worker** panic: the gate is `true`, the hook is a no-op, the
  live terminal is untouched, and `spawn_worker`'s `catch_unwind` turns the
  panic into the fallback event.
- `run_with_model`/`run_loop` run on the OS main thread (`main.rs` calls
  `tui::run`/`run_demo` directly, no intervening spawn), so the line-63
  `ratatui::restore()` + trailing `take_hook()` (which reinstalls std's default
  hook) run on the normal-return path; on the panic path the process is already
  unwinding out, so leaving the hook is harmless.

## Pivot: what we deliberately do NOT change

- **Keep the four `.expect()` calls in probe.rs** (116, 268, 344, 346). They
  document a validated-upstream invariant. With `catch_unwind` in place, a
  hypothetical violation now renders a clean `PoolStatus::Error` instead of
  hanging. Downgrading them to `continue` (as the original finding proposed)
  would silently drop a disk row from the table for a can't-happen case --
  strictly worse diagnostics, and it contradicts probe.rs's deliberate
  "surface explicitly, never silently swallow" ethos (e.g. the exhaustive
  `ProbeError` arm at lines 370-382).
- **Do NOT re-type `DiskIdentity`** to hold `DiskName`/`ByIdPath` instead of
  `String`. That would ripple through the String-keyed model and view layer
  (`disk_usage`, `smart_health`, `disk_transport`, etc. are all
  `HashMap<String, _>`) for no real benefit, since the round-trip is provably
  idempotent and the can't-happen case is now caught.

## Out of scope (stated, not silently dropped)

- **The input-reader thread (`event.rs:82`)** is a 7th `thread::spawn` that is
  **not** wrapped by `spawn_worker`. A panic there loses keyboard input rather
  than stranding a probe subsystem -- a different failure mode, outside this
  finding. Because it does not set `IN_CAUGHT_WORKER`, our gate leaves it on
  ratatui's normal restore-then-print path, so its terminal behavior is
  unchanged from today (stock ratatui). Do not claim "all spawns are
  panic-safe" -- this one is intentionally only strand-unprotected, not
  terminal-unsafe.

## Tests

`execute_effect`'s workers hardcode `RealRunner`/`RealFilesystem` inside the
closure and are not unit-testable; the probe **logic** is already covered by
`probe.rs`'s `MockRunner` tests. The **new** logic to test is the panic-safety
net, and `spawn_worker` is the clean seam (pure: a `Sender` + two closures).
Add a `#[cfg(test)] mod tests` to effect.rs (none today):

- **Panic body -> fallback delivered, and the caught-worker flag is set.**
  `spawn_worker(&tx, || { assert!(in_caught_worker(), "worker thread must be
  marked caught"); panic!("boom") }, |msg| Event::PoolProbeFinished(Err(msg),
  Duration::ZERO))`, then `rx.recv_timeout(Duration::from_secs(5))` and assert
  (structurally -- `Event` has no `PartialEq`) a `PoolProbeFinished(Err(e), _)`
  arrives with `e` containing `"boom"` (proves `panic_message` downcast works).
  The in-body `assert!(in_caught_worker())` runs before the panic on the worker
  thread, so deleting the `set(true)` in `spawn_worker` flips the fallback
  message away from `"boom"` and fails this test. This guards the **producer**
  half of the terminal-safety contract (the flag is set); it is a mechanism
  proxy, not a substitute for smoke step (c), which exercises the **consumer**
  (the hook actually leaving the terminal intact). Use `recv_timeout`, never
  bare `recv`, so a regression fails instead of hanging.
- **Non-panic body -> body's event delivered.** `spawn_worker(&tx, ||
  Event::PollFanRefresh, |_| Event::PollUpsRefresh)`; assert
  `matches!(rx.recv_timeout(..), Ok(Event::PollFanRefresh))`.
- Expect a "thread panicked at boom" line on stderr during the panic test (the
  test process has no gating hook installed, so std's default hook prints); that
  is harmless test noise.

**Coverage limit (acknowledged, not closed):** these tests prove `spawn_worker`
in isolation, but nothing fails if a future edit (or a missed site in this
change) leaves one of the six `execute_effect` spawns as a raw `thread::spawn`
-- the real runners are hardcoded, so the per-site routing is not behaviorally
testable. The wiring of all six sites through `spawn_worker` is verified by code
review, not by a test. Injecting a spawn seam into `execute_effect` purely to
test this is not worth the dispatcher churn.

Producer vs consumer: the gate's **producer** (does `spawn_worker` mark its
thread caught?) is unit-tested above. The gate's **consumer** (does the hook
actually leave the live terminal intact for a caught panic, and restore for a
main-thread panic?) needs a real terminal and is validated by the mechanics
above plus manual smoke step (c)/(d) -- it cannot be unit-tested.

## Verification

1. `just test-rust` -- the gate. New effect.rs tests run under `--lib`; the
   existing app.rs reschedule tests and the insta view snapshots
   (`cli/src/tui/view/snapshots/`) run here too.
2. `just clippy` -- must stay clean. Watch `clippy::borrowed_box` on
   `panic_message` (hence the `&(dyn Any + Send)` signature, not `&Box<...>`).
3. **Snapshot tests should not change** -- the view is untouched and an
   all-`None`/`Unknown` snapshot renders identically to the pre-probe `None`
   state. If any `.snap` diff appears, investigate rather than blindly accept.
4. **Manual smoke (dev only):** in a scratch build, inject `panic!("probe
   boom")` into a probe body, run `braid tui` over SSH, and confirm: (a) the
   pool view shows the error banner (not a perpetual spinner), (b) the fan/UPS
   cadence keeps ticking (the loop revived), (c) the TUI stays fully intact --
   no terminal teardown and no stray panic text on the alternate screen (this is
   the regression the hook gate prevents -- without it, ratatui's `restore()`
   would fire on the caught panic), and (d) a `panic!` on the **main** thread
   still restores the terminal and prints the message on a clean screen.

## Critical files

- `cli/src/tui/effect.rs` -- add `panic_message` + `spawn_worker`; rewrite all 6
  spawn sites; add the test module.
- `cli/src/tui/model.rs` -- add `FanSnapshot::unknown()` and
  `UpsSnapshot::unknown()`.
- `cli/src/tui/mod.rs` -- gate ratatui's panic hook by `IN_CAUGHT_WORKER` in
  `run_with_model`, and reset it on normal exit.
- Unchanged but load-bearing context: `cli/src/tui/app.rs` (handlers that make
  the fallbacks work), `cli/src/tui/event.rs` (`Event` variants /
  `into_message`), `cli/src/tui/probe.rs` (the four `.expect()`s left as-is).
