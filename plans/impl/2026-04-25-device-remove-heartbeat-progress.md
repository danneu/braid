# remove-missing: emit visible progress during btrfs device remove

## Context

`braid remove-missing` executes `btrfs device remove <devid> <mount>` via
`pool_remove_devid` (`cli/src/pool.rs:204`), which calls `runner.run(...)`
directly and does not thread the caller's `ProgressOutput`. On a 3+ device
pool with data allocated to the missing device, `btrfs device remove` runs
chunk relocation via `btrfs_shrink_device`
(`reference/linux/fs/btrfs/volumes.c:4844`) and can take minutes. During
that window the operator sees no output -- `params.progress` is only
consumed later, inside `maybe_restore_raid1`'s soft balance.

A straightforward "reuse `run_with_progress`" fix does not work here.
`run_with_progress` polls `btrfs balance status`, which calls
`BTRFS_IOC_BALANCE_PROGRESS`. That ioctl handler is
`btrfs_ioctl_balance_progress` (`reference/linux/fs/btrfs/ioctl.c:3705`)
and returns `-ENOTCONN` whenever `fs_info->balance_ctl` is NULL
(`reference/linux/fs/btrfs/ioctl.c:3715`). The device-remove path --
`btrfs_rm_device` (`reference/linux/fs/btrfs/volumes.c:2153`) calling
`btrfs_shrink_device` (`reference/linux/fs/btrfs/volumes.c:2210` ->
defined at `volumes.c:4844`) -- never populates `balance_ctl`; it runs
chunk relocation under its own exclusive-op marker
(`BTRFS_EXCLOP_DEV_REMOVE`). So `btrfs balance status` keeps returning
"No balance found" for the entire remove, and `run_with_progress`'s
current code writes nothing. The existing live-device remove path
(`evict_present_device` -> `pool_remove_device` -> `run_with_progress`)
has the same latent bug for exactly the same reason; the fix below
improves both paths.

Decision 019 compounds the confusion by labeling this phase "fast
metadata-only" (`docs/decisions/019-inhibit-sleep.md:92`). The upstream
semantics do not match -- the surviving RAID1 stripes on other devices
get rewritten into newly allocated chunks on remaining devices, which
is real I/O work.

Outcome: `braid remove-missing` (and `braid remove` of a live device)
shows a visible, honest "still working" signal on stderr throughout the
remove phase, the code stops carrying a second non-progress remove
helper, and the sleep-inhibitor decision record matches the actual
duration model.

## Design

The kernel does not expose a percent-complete or chunk-count counter we
can poll for `btrfs device remove`, so the best faithful signal is an
**elapsed-time heartbeat**: one line every N seconds while the remove
worker is running. Human mode rewrites a single stderr line via
`\r\x1b[K`; JSON mode emits one event per tick. When the remove returns,
the line gets cleared the same way `run_with_progress` already clears its
own.

To avoid mixing concerns and regressing the balance callers (which do
report real progress), this is a separate helper, not a conditional
added to `run_with_progress`:

```
run_device_remove_with_progress(runner, request, output)
  // internally: thread::scope with a worker running runner.run(request)
  //             and a main loop that sleeps, increments elapsed, emits heartbeat
```

Both testability and determinism come from an injected `Sleeper` trait --
same pattern and name as `cli/src/lock.rs:29`. The implementation delegates
to `run_device_remove_with_progress_using(runner, request, output, sleeper, sink)`;
production constructs `RealSleeper` + a stderr-backed sink, tests construct
a noop sleeper + a recording sink.

`ProgressOutput::Off` stays the escape hatch: it short-circuits to a
plain `runner.run(request)`, matching `run_with_progress`'s behavior and
keeping existing tests that pass `Off` unchanged.

## Exact changes

### `cli/src/progress.rs`

0. Update the stale doc comment at lines 141-142. Today it says:

   ```
   /// Run a blocking btrfs command with progress polling.
   /// Works for BtrfsBalanceRaid1, BtrfsBalanceSingle, and BtrfsDeviceRemove.
   ```

   The `BtrfsDeviceRemove` claim is wrong -- this helper polls
   `btrfs balance status`, which is empty for the duration of
   `btrfs device remove` (the kernel path does not populate
   `balance_ctl`; see Context). Replace with:

   ```
   /// Run a blocking balance-driven btrfs command with progress
   /// polling via `btrfs balance status`.
   /// Works for BtrfsBalanceRaid1, BtrfsBalanceSingle, and the
   /// BtrfsBalance* variants. NOT suitable for BtrfsDeviceRemove --
   /// device remove uses its own exclusive-op path and does not
   /// surface in balance status; route those through
   /// `run_device_remove_with_progress` instead.
   ```

1. Introduce the shared injection traits. `cli/src/lib.rs` exports
   both `pub mod pool` and `pub mod progress`, so `pub` items inside
   them join the external crate API. Visibility is tuned per item:

   - `Sleeper`, `RealSleeper`: **`pub`** -- they appear in the
     `RemoveMissingParams::sleeper` field type (a `pub` struct on a
     `pub mod`), and `RealSleeper` is constructed in `main.rs`
     (`&braid_cli::progress::RealSleeper`).
   - `NoopSleeper`: **`pub(crate)`** -- only constructed in
     in-crate test modules.
   - `ProgressSink`, `StderrSink`: **`pub(crate)`** -- internal
     seam, no production caller outside the crate ever names these.
   - `run_device_remove_with_progress`,
     `run_device_remove_with_progress_using`: **`pub(crate)`** --
     same; called only from `pool.rs`.

   ```rust
   pub trait Sleeper: Sync {
       fn sleep(&self, duration: Duration);
   }
   pub struct RealSleeper;
   impl Sleeper for RealSleeper {
       fn sleep(&self, d: Duration) { std::thread::sleep(d); }
   }
   pub(crate) struct NoopSleeper;
   impl Sleeper for NoopSleeper {
       fn sleep(&self, _: Duration) {}
   }

   pub(crate) trait ProgressSink: Sync {
       fn write_line(&self, msg: &str);  // Human mode; rewriting line
       fn write_json(&self, msg: &str);  // Json mode; newline-delimited
       fn clear(&self);                  // end-of-run, clear rewritable line
   }
   pub(crate) struct StderrSink;
   impl ProgressSink for StderrSink {
       fn write_line(&self, m: &str) { write_progress_line(m); }
       fn write_json(&self, m: &str) { write_progress_json(m); }
       fn clear(&self) { clear_progress_line(); }
   }
   ```

   `Sleeper` deliberately matches the trait name in `cli/src/lock.rs:29`
   (distinct module path, no collision). Reuse the existing private
   `write_progress_line` / `write_progress_json` / `clear_progress_line`
   helpers inside `StderrSink`.

2. Add `HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);` as a
   module-level constant. 5s rather than 1s because a heartbeat with
   no quantitative content is pure liveness signal -- 1s rewrites would
   be noisier than useful. (Name is fine to tune in review; pick
   something deterministic so one test pins the prod cadence.)

3. Add `format_device_remove_heartbeat(elapsed: Duration) -> String` and
   `format_device_remove_heartbeat_json(elapsed: Duration) -> String`.
   Human form: `"  device remove: working (Ns elapsed)"`. JSON form:
   `{"event":"device_remove_heartbeat","elapsed_secs":N}`. Keep them pure
   so they can be unit-tested without the runner scaffolding.

4. Add the new helper pair:

   ```rust
   pub(crate) fn run_device_remove_with_progress<R: CommandRunner + Sync>(
       runner: &R,
       request: &CmdRequest,
       output: ProgressOutput,
   ) -> Result<RawCommandOutput, CmdError> {
       run_device_remove_with_progress_using(
           runner, request, output, &RealSleeper, &StderrSink,
       )
   }

   pub(crate) fn run_device_remove_with_progress_using<R, S, W>(
       runner: &R,
       request: &CmdRequest,
       output: ProgressOutput,
       sleeper: &S,
       sink: &W,
   ) -> Result<RawCommandOutput, CmdError>
   where
       R: CommandRunner + Sync,
       S: Sleeper + ?Sized,
       W: ProgressSink + ?Sized,
   { /* ... */ }
   ```

   `?Sized` is mandatory on both `S` and `W`: callers pass
   `params.sleeper: &dyn progress::Sleeper` (an unsized trait
   object) through `pool_remove_device_using` into this helper. The
   default `Sized` bound would reject those callsites at the
   monomorphization seam. (Same reason `&dyn`-carrying APIs
   elsewhere in this crate use `?Sized` bounds.) Both functions are
   `pub(crate)` -- only `pool.rs` calls them.

   Loop body:
   - `if output == Off { return runner.run(request); }`
   - `thread::scope` with `handle = s.spawn(|| runner.run(request))`.
   - `let mut elapsed = Duration::ZERO;` and
     `let mut last_msg = String::new();`
     (Track elapsed as an accumulator incremented by
     `HEARTBEAT_INTERVAL` after each sleep, **not** `Instant::now()`.
     With an injected no-op `Sleeper`, real wall-clock never advances,
     so `Instant::now()` would always return ~0 and the emitted
     heartbeat text would be non-deterministic in tests. Accumulating
     elapsed makes the rendered line a pure function of sleep-call
     count.)
   - Loop: `if handle.is_finished() { break; }`
     `sleeper.sleep(HEARTBEAT_INTERVAL);`
     `elapsed += HEARTBEAT_INTERVAL;`
     **`if handle.is_finished() { break; }`** (second check, required:
     the worker can finish *during* the sleep, and without the
     recheck we would emit a heartbeat after completion -- in human
     mode a transient line the caller's clear would race against, in
     JSON mode a misleading post-completion event committed to the
     stream). Then:
     - `Human`: `let msg = format_device_remove_heartbeat(elapsed); if msg != last_msg { sink.write_line(&msg); last_msg = msg; }`
     - `Json`: `sink.write_json(&format_device_remove_heartbeat_json(elapsed));`
     - `Off`: unreachable (early return above).
   - After break: `if output == Human { sink.clear(); }` then `handle.join().expect(...)`.

   Do NOT poll `btrfs balance status` here -- per the kernel analysis
   above, it is useless for device remove and a negative signal that
   would suggest otherwise.

### `cli/src/pool.rs`

5. Delete `pool_remove_devid` (lines 203-221). Nothing else references
   it (`grep -rn 'pool_remove_devid' cli/`).

6. Split `pool_remove_device` into a public convenience and a
   `pub(crate)` `_using` variant that accepts the injected `Sleeper`
   and `ProgressSink`. This is the seam the E2 behavioral test
   observes -- without it, there is no way for a test in a different
   module to drive `pool_remove_device` with a fake sleeper and
   recording sink.

   ```rust
   pub fn pool_remove_device<R: CommandRunner + Sync>(
       runner: &R,
       device: &str,
       mount_point: &MountPoint,
       progress: ProgressOutput,
   ) -> Result<(), PoolError> {
       pool_remove_device_using(
           runner, device, mount_point, progress,
           &progress::RealSleeper, &progress::StderrSink,
       )
   }

   pub(crate) fn pool_remove_device_using<R, S, W>(
       runner: &R,
       device: &str,
       mount_point: &MountPoint,
       progress: ProgressOutput,
       sleeper: &S,
       sink: &W,
   ) -> Result<(), PoolError>
   where
       R: CommandRunner + Sync,
       S: progress::Sleeper + ?Sized,
       W: progress::ProgressSink + ?Sized,
   {
       let result = progress::run_device_remove_with_progress_using(
           runner,
           &CmdRequest::BtrfsDeviceRemove {
               device: device.to_owned(),
               mount_point: mount_point.clone(),
           },
           progress,
           sleeper,
           sink,
       )?;
       // existing exit-status handling unchanged
   }
   ```

   `?Sized` matches the helper signature in step 4. `pub(crate)`
   because the only callers are `remove_missing::execute` and
   in-crate test modules; the public crate API surface stays the
   `pool_remove_device` convenience.

   All existing `pool_remove_device` callers (`evict_present_device`
   in this file, and the remove-missing callsite in step 9) continue
   to use the public convenience; only tests reach for `_using`.

   Balance helpers in this file are unchanged; the `run_with_progress`
   import stays (they still call it).

### `cli/src/remove_missing.rs`

7. Line 8: `use crate::pool::pool_remove_devid;` ->
   `use crate::pool::pool_remove_device_using;`.

8. Add a `sleeper` field to `RemoveMissingParams`, mirroring the
   existing `sleep_inhibitor` injection seam (which already lives on
   the same struct for the same reason -- letting tests skip a
   wall-clock-bound subprocess). Same `&'a dyn` shape:

   ```rust
   pub struct RemoveMissingParams<'a> {
       // ... existing fields ...
       pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
       /// Seam for the heartbeat poll loop in
       /// `pool_remove_device_using`. Production passes
       /// `&progress::RealSleeper`; tests pass `&progress::NoopSleeper`
       /// (or equivalent) so the device-remove progress test does
       /// not pay a real `HEARTBEAT_INTERVAL` wall-clock wait.
       pub sleeper: &'a dyn progress::Sleeper,
   }
   ```

   For this to work, `progress::Sleeper` becomes `pub` (not
   `pub(crate)`) and the trait gets `?Sized` accommodation -- exactly
   the same shape `AcquireSleepInhibitor` already has. Add a
   `pub struct NoopSleeper;` next to `RealSleeper` in `progress.rs`
   for test use.

9. Lines 155-161: rewrite the inhibitor-scope comment. Drop "fast
   metadata-only". New wording, approximately:

   ```
   // Hold a logind sleep inhibitor for the rest of the remove-missing
   // operation -- covers the btrfs device remove (chunk relocation;
   // can run for minutes when the missing device had data allocated)
   // and the post-op maybe_restore_raid1 soft balance ...
   ```

10. Line 195: replace
    ```rust
    pool_remove_devid(runner, &self.mount_point, resolved_devid)?;
    ```
    with
    ```rust
    pool_remove_device_using(
        runner,
        &resolved_devid.to_string(),
        &self.mount_point,
        params.progress,
        params.sleeper,
        &progress::StderrSink,
    )?;
    ```

    `pool_remove_device_using` is the seam (step 6); calling it
    directly here is what threads `params.sleeper` through. Production
    sink is hard-coded to `StderrSink` because no current caller has
    a reason to override it; if a future test needs to capture the
    sink output at the `cmd_remove_missing` layer, add a sink field
    by the same pattern.

    `btrfs device remove <device>|<devid>` accepts both operand forms
    per `reference/btrfs-progs/Documentation/btrfs-device.rst`, and
    the rendered `btrfs device remove --enqueue <devid> <mount>` is
    byte-identical to what `compile_steps` already emits for the
    dry-run preview at `cli/src/remove_missing.rs:483`.

11. Update every existing `RemoveMissingParams { ... }` constructor
    found by:

    ```
    rg -n 'RemoveMissingParams \{' cli/src/remove_missing.rs cli/src/main.rs
    ```

    Current count is 9 in `remove_missing.rs` (test sites) plus 1 in
    `main.rs` (production); confirm with the rg run before applying
    so no callsite is missed (compile failures otherwise). Tests
    take `sleeper: &progress::NoopSleeper,`; almost all of them pass
    `progress: ProgressOutput::Off`, so the sleeper is never reached
    in practice -- the field exists only to satisfy the struct shape
    and to give the new (e) test a real seam.

### `cli/src/main.rs`

12. The single production callsite (currently `cli/src/main.rs:361`,
    re-grep before editing in case lines have shifted) constructs
    `RemoveMissingParams { ... }`. Add
    `sleeper: &braid_cli::progress::RealSleeper,` (matching the
    existing `sleep_inhibitor: &braid_cli::inhibit::RealSleepInhibitor`
    line).

### `docs/decisions/019-inhibit-sleep.md`

10. Line 92: replace `pool_remove_devid (fast metadata-only)` with a
    bullet that names the real phase and its cost, e.g.:

    ```
    - `btrfs device remove <devid>` (chunk relocation via
      `btrfs_shrink_device`; can run for minutes when the missing
      device had data allocated -- surviving RAID1 stripes get
      rewritten into newly allocated chunks on remaining devices)
    ```

    The surrounding "acquire before journal" rule is unchanged.

### `tests/cli/remove-missing-inhibits-suspend.py`

11. Lines 22-23 carry the same stale "metadata-only since the device
    is already gone" claim. Update the comment to match reality. The
    assertions below don't depend on duration and are unaffected; this
    is a comment-only edit to keep misreadings from propagating.

## Tests

### `cli/src/progress.rs` -- new unit module scoped to the new helper

a. `format_device_remove_heartbeat` pure-format tests (three cases:
   0s, 7s, 2m). Lock prod wording.

b. `format_device_remove_heartbeat_json` pure-format test. Lock the
   JSON shape (`{"event":"device_remove_heartbeat","elapsed_secs":N}`).

c. **Behavioral test that pins both the wiring AND the output**:

   ```rust
   struct RecordingSink {
       lines: Arc<Mutex<Vec<String>>>,
       jsons: Arc<Mutex<Vec<String>>>,
       clears: Arc<AtomicUsize>,
   }
   impl ProgressSink for RecordingSink { /* push to vectors */ }

   struct FakeSleeper {
       calls: Arc<Mutex<Vec<Duration>>>,
   }
   impl Sleeper for FakeSleeper {
       fn sleep(&self, d: Duration) {
           self.calls.lock().unwrap().push(d);
           // do NOT actually sleep
       }
   }

   /* Intent: the device-remove progress helper emits a human
    * heartbeat while the worker is running and clears on completion.
    * Why it exists: the original bug was that `btrfs device remove`
    * produced no operator output on slow pools. This test proves
    * the helper writes at least one heartbeat line to the sink and
    * then clears it, deterministically and without real sleeps.
    * Scenario: a mock runner whose BtrfsDeviceRemove blocks until
    * the test thread observes one FakeSleeper tick, then returns.
    */
   #[test]
   fn device_remove_emits_heartbeat_human() { /* ... */ }

   /* Twin test for ProgressOutput::Json using write_json pathway. */
   #[test]
   fn device_remove_emits_heartbeat_json() { /* ... */ }

   /* Intent: ProgressOutput::Off short-circuits to runner.run with
    * no heartbeat emission. */
   #[test]
   fn device_remove_off_emits_nothing() { /* ... */ }

   /* Intent: the helper asks the Sleeper for HEARTBEAT_INTERVAL,
    * pinning prod cadence. Prevents silent drift if someone edits
    * the const without updating docs. */
   #[test]
   fn device_remove_sleeps_at_configured_interval() { /* ... */ }
   ```

   Synchronization detail for test (c): the fake runner's
   `BtrfsDeviceRemove` handler blocks on a condvar that the
   `RecordingSink::write_line` call flips. Net effect: the helper
   loop sleeps (FakeSleeper returns immediately), advances `elapsed`
   by the configured interval, writes the heartbeat, sink flips the
   condvar, runner unblocks. No real wall-clock time is consumed.

   If that handshake is too ornate, a simpler variant: the runner
   blocks on a `Barrier` that the TEST thread trips right after
   spawning `run_device_remove_with_progress_using` on a worker
   thread. The invariant checked is identical -- at least one
   heartbeat line was written before the remove completed.

### `cli/src/pool.rs` -- E2 behavioral wiring test (helper layer)

d. The E2 test sits in `pool.rs`'s `#[cfg(test)] mod tests` and calls
   `pool_remove_device_using` (added in step 6) directly with
   `ProgressOutput::Human`, a `FakeSleeper`, and a `RecordingSink`.
   Mock runner shape:

   - `BtrfsDeviceRemove`: returns success after the test thread
     observes the first heartbeat line. Cleanest implementation: the
     runner's `run` for `BtrfsDeviceRemove` blocks on a `(Mutex<bool>,
     Condvar)` pair that the `RecordingSink::write_line` impl flips.
     With a `FakeSleeper` whose `sleep` is a no-op, the helper loop
     advances `elapsed` to `HEARTBEAT_INTERVAL` and writes one
     heartbeat line on the first iteration; the sink flip unblocks
     the runner; remove returns; loop exits.

   Assertions:
   - `sink.lines()` has length >= 1 (a heartbeat *was* emitted).
   - The first recorded line equals
     `format_device_remove_heartbeat(HEARTBEAT_INTERVAL)` byte-for-byte.
     (Locks the prod wording AND the elapsed-accumulator behavior in
     one assertion -- if elapsed accidentally returned to using
     `Instant::now()`, the first emitted line would be `"... (0s
     elapsed)"` and the assertion would fire.)
   - `sink.clears()` == 1 (the post-loop clear ran).
   - `sleeper.calls()` shows at least one entry equal to
     `HEARTBEAT_INTERVAL`.

   This proves the E2 wiring contract end-to-end at the layer that
   owns the heartbeat: `pool_remove_device -> pool_remove_device_using
   -> run_device_remove_with_progress_using`. If anyone reverts the
   wiring (e.g. has `pool_remove_device` call `runner.run` directly),
   the sink stays empty and the test fails on the first assertion.

### `cli/src/remove_missing.rs` -- cmd-layer wiring test

e. The (d) test only proves that `pool_remove_device_using` writes a
   heartbeat. It does not prove that `cmd_remove_missing` actually
   *reaches* `pool_remove_device_using` with `params.progress` and
   `params.sleeper` threaded through. Without (e), a regression
   that bypassed the helper inside `RemoveMissingPlan::execute` (e.g.
   `runner.run(BtrfsDeviceRemove)` direct, or
   `pool_remove_device_using(... ProgressOutput::Off, ...)`) would
   leave (d) green and the user-visible bug unfixed.

   Add a behavioral test in `remove_missing.rs::tests` that observes
   **the thread that `BtrfsDeviceRemove` ran on**. The new helper's
   `Off` branch returns `runner.run(request)` on the calling thread;
   its `Human`/`Json` branch dispatches the runner call onto a
   `thread::scope`-spawned worker thread. Comparing the recorded
   thread id against the calling thread id is therefore a faithful
   proxy for "did this go through the progress helper?":

   ```rust
   /* Intent: cmd_remove_missing routes the device-remove phase
    * through the progress helper, not a direct runner.run, when
    * params.progress is non-Off.
    * Why it exists: without this guard, future edits to
    * RemoveMissingPlan::execute could drop params.progress (e.g.
    * by calling the bare `pool_remove_device` convenience that
    * always pairs with RealSleeper, or by calling runner.run
    * directly) and the user-visible silence bug would return.
    * Scenario: 3-disk pool, 1 missing, ProgressOutput::Human; the
    * mock runner records the thread id for BtrfsDeviceRemove and
    * the test asserts that id is not the calling thread.
    */
   #[test]
   fn device_remove_runs_on_progress_worker_thread() {
       // ... three_device_config setup as in the existing tests ...
       let calling_thread = std::thread::current().id();
       let recorded: Arc<Mutex<Option<std::thread::ThreadId>>> =
           Arc::new(Mutex::new(None));
       let runner = ThreadIdRecordingRunner::new(Arc::clone(&recorded));
       let inhibitor = crate::inhibit::RecordingInhibitor::new();
       let sleeper = crate::progress::NoopSleeper;
       cmd_remove_missing(
           &runner,
           &MockFs,
           &RemoveMissingParams {
               // ... existing fields ...
               progress: crate::progress::ProgressOutput::Human,
               sleep_inhibitor: &inhibitor,
               sleeper: &sleeper,
           },
       ).expect("remove-missing should succeed");
       let observed = recorded.lock().unwrap().expect(
           "BtrfsDeviceRemove must have been dispatched at least once",
       );
       assert_ne!(
           observed, calling_thread,
           "BtrfsDeviceRemove must run on the progress helper's worker \
            thread when ProgressOutput::Human is threaded through; \
            running on the calling thread means the helper was bypassed",
       );
   }
   ```

   `ThreadIdRecordingRunner` is a file-local mock that records
   `std::thread::current().id()` inside its `BtrfsDeviceRemove`
   handler and otherwise mirrors the existing
   `ThreeDeviceRunner` (probes, balance-status no-op, etc.). It does
   not need a condvar -- because the helper uses `NoopSleeper`, the
   loop's first `is_finished()` check, the no-op sleep, and the
   second `is_finished()` check all return without blocking, so the
   total wall-clock cost is dominated by spawning the scoped thread
   (~milliseconds).

   Note: this test's `still_degraded_after = true` mirrors the
   existing `three_device_two_missing_no_rebalance`, so
   `maybe_restore_raid1` is a no-op and the only observed
   `BtrfsDeviceRemove` is from the device-remove phase. That keeps
   the test focused on the failure layer the issue identifies.

   Existing `three_device_pool_*` tests stay on `ProgressOutput::Off`
   and are unchanged. They continue to lock in the journal/inhibitor
   contracts.

### Existing `remove_missing.rs` tests

All pass `ProgressOutput::Off`, which short-circuits the new helper
identically to the old one (`runner.run(request)`). No edits required.

### Dry-run render test (`dry_run_render_targeted_removal_with_balance`)

Already pins `btrfs device remove --enqueue 2 /mnt/storage`. The
command is built by `compile_steps` (unchanged) and matches the
real-run emission byte-for-byte. No edit needed.

## Open questions / risks

1. **Heartbeat interval.** 5s is a guess at "often enough to feel
   alive, rare enough to not spam." The pinning test in (c) locks the
   prod value so any edit is explicit. If review prefers 2s or 10s,
   change `HEARTBEAT_INTERVAL` and the test in one commit.

2. **JSON consumers.** `ProgressOutput::Json` is an existing public
   event stream; adding a new event type
   (`"event":"device_remove_heartbeat"`) is additive. No existing JSON
   consumer in-tree relies on "device remove is silent"; consumers
   parsing balance events will ignore unknown event types. Called out
   here so review can confirm.

3. **`evict_present_device` behavior change.** Routing
   `pool_remove_device` through the new helper changes `braid remove`
   (live device) stderr too -- it now emits heartbeats instead of
   silence during the remove phase. That is an improvement and matches
   what the function already claimed to do, but it is a behavior change
   and should be called out in the PR description. No existing VM test
   for `braid remove` asserts the absence of stderr chatter; a quick
   grep of `tests/cli/braid-remove-disk.py` for `progress` /
   `heartbeat` / `assert_matches` in stderr scope is the final safety
   check before merging.

4. **Still-running heartbeat on very fast removes.** If `BtrfsDeviceRemove`
   returns before the first `HEARTBEAT_INTERVAL` tick, no heartbeat is
   emitted. That is correct -- the loop checks `handle.is_finished()`
   first. Existing fast-path behavior (no output for a fast operation)
   is preserved.

5. **`run_with_progress` untouched.** Balance callers
   (`pool_balance_raid1`, `pool_balance_single`,
   `pool_balance_raid1_soft`, `pool_balance_resume`, `pool_replace_device`
   via `run_replace_with_progress`) keep their current behavior. No
   regression risk to balance progress.

## Recommended minimal patch sequence

1. `cli/src/progress.rs`: update the stale `run_with_progress` doc
   comment (step 0); add `Sleeper`/`RealSleeper`/`NoopSleeper`,
   `ProgressSink`/`StderrSink`, `HEARTBEAT_INTERVAL`, the two
   formatters, and `run_device_remove_with_progress{,_using}` with
   the two-phase `is_finished()` check and accumulator-based
   elapsed. Add the four pure/behavioral unit tests listed under
   (a)-(c).
2. `cli/src/pool.rs`: delete `pool_remove_devid`; split
   `pool_remove_device` into the public convenience and
   `pool_remove_device_using`; route the convenience through
   `_using` with `RealSleeper` + `StderrSink`. Add the E2 behavioral
   heartbeat test (d) on `pool_remove_device_using`.
3. `cli/src/remove_missing.rs`: switch import to
   `pool_remove_device_using`, add the `sleeper` field to
   `RemoveMissingParams`, rewrite the inhibitor-scope comment, and
   add the `device_remove_runs_on_progress_worker_thread` test (e).
   Update every existing `RemoveMissingParams { ... }` constructor
   in `cli/src/remove_missing.rs` (re-confirm count via
   `rg -n 'RemoveMissingParams \{' cli/src/remove_missing.rs` --
   currently 9) to include `sleeper: &progress::NoopSleeper,`.
4. `cli/src/main.rs`: add `sleeper: &braid_cli::progress::RealSleeper,`
   to the production `RemoveMissingParams` constructor at line 361.
5. `docs/decisions/019-inhibit-sleep.md`: update line 92.
6. `tests/cli/remove-missing-inhibits-suspend.py`: update the stale
   comment at lines 22-23.
7. `just test-rust` (repo-standard Rust verification recipe; the
   justfile entry runs `cargo test` with the project's pinned
   toolchain). Targeted `cargo test -p braid-cli` is fine as a local
   tight loop during iteration but the merge gate is `just test-rust`.
8. `just test-vm remove-missing-inhibits-suspend` to confirm the
   comment-only VM test edit still passes.

## Verification

- Unit: `just test-rust` passes, including the new
  `device_remove_emits_heartbeat_{human,json}`,
  `device_remove_off_emits_nothing`,
  `device_remove_sleeps_at_configured_interval`,
  `pool_remove_device_using_emits_heartbeat` (d), and
  `device_remove_runs_on_progress_worker_thread` (e) tests.
- Helper-layer regression guard (d): temporarily make
  `pool_remove_device_using` bypass
  `run_device_remove_with_progress_using` and call
  `runner.run(BtrfsDeviceRemove)` directly. Confirm (d) fails on the
  "first recorded heartbeat line" assertion. Re-apply.
- Cmd-layer regression guard (e): temporarily replace the `params.progress`
  argument in `RemoveMissingPlan::execute`'s
  `pool_remove_device_using` call with `ProgressOutput::Off`. Confirm
  (e) fails with the "must run on the progress helper's worker
  thread" assertion. Re-apply.
- Post-completion-emit guard: temporarily delete the second
  `if handle.is_finished() { break; }` recheck after the sleeper call
  and confirm the JSON-mode heartbeat test catches the misleading
  post-completion event. Re-apply.
- VM: `just test-vm remove-missing-inhibits-suspend` passes
  (comment-only change).
- Manual sanity (optional): in a VM shell with a 3-disk pool that has
  data allocated to the missing device, run
  `braid remove-missing --missing-id <N>` and observe a
  `  device remove: working (Ns elapsed)` line rewriting on stderr at
  the configured interval until the remove completes.
