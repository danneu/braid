# Fix the transient failure in braid-idle's "scrub genuinely running" subtest

## Context

`tests/cli/braid-idle.py`, subtest **"braid idle reports busy while a scrub is
genuinely running"**, fails intermittently (it failed in the latest `just
test-vm -k` run with `running scrub must make braid idle exit 1, got 0: idle:
pool is idle`).

Root cause (proven against the vendored sources, not guessed):

- `btrfs scrub status` prints a `Status:` line **only** once the scrub *daemon*
  has surfaced a progress record with a non-zero `t_start`. Before that,
  `_print_scrub_ss` (`reference/btrfs-progs/cmds/scrub.c`) emits "no stats
  available" with no `Status:` line. The daemon surfaces that record on a
  **~5s cycle** (`scrub_progress_cycle`: `poll(&accept_poll_fd, 1, 5 * 1000)`),
  and the initial on-disk status file is written with `t_start=0` too
  (`scrub_write_progress` precedes the `t_start` assignment). So `Status:
  running` first appears ~5s after `btrfs scrub start`, regardless of scrub
  progress.
- The current test throttles reads with `read_delay_ms=500` over a 32 MiB
  payload, so the whole scrub ran only ~5.46s. "running" surfaced at ~5.2s and
  the kernel cleared `dev->scrub_ctx` ~0.16s later. `braid idle` then spawned
  its own `btrfs scrub status`, which landed *after* completion, parsed
  `Finished`, and returned `idle: pool is idle` (exit 0). It is intermittent
  because it hinges on whether the single `braid idle` sample beats scrub
  completion in that ~160ms window.
- braid maps the pre-surfacing "no stats available" to `ScrubState::Never` ->
  `IdleResult::Idle` (exit 0) (`cli/src/idle.rs#cmd_idle`,
  `cli/src/parse/btrfs_scrub_status.rs#parse_btrfs_scrub_status`). So a test
  **must** wait for `Status: running` before sampling `braid idle` -- a naive
  early sample reads as idle.

Intended outcome: make the subtest deterministic by **freezing** the scrub's
reads so it cannot reach a terminal state until cleanup releases them. The
`Status: running` gate stays, but the post-gate sample is then race-free instead
of riding a timing margin. The `braid idle` behavior under test does not change
(the Rust logic is already correct and unit-pinned by
`busy_when_scrub_running`); only the VM test's scrub-holding mechanism changes.

## Approach: freeze the scrub instead of throttling it

`braid idle` and `btrfs scrub status` only touch the socket / ioctl / rootfs
status path -- never the throttled data device (`braid idle` runs `btrfs scrub
status --raw <mount>`, `cli/src/cmd.rs#CmdRequest::BtrfsScrubStatus`). So we can
block the scrub's data reads outright while keeping the observers fast. With
reads blocked the scrub stays registered (`dev->scrub_ctx` non-null), so once
`Status: running` surfaces it stays running until cleanup -- no race.

Key parameter and structure choices:

- **`read_delay_ms=600000`** (effectively a freeze; released in cleanup). The
  freeze is held only ~5-10s in the happy path (start -> ~5s surfacing -> one
  sample -> release), well under the kernel default `hung_task_timeout_secs=120`,
  so no hung-task warning.
- **Raise the payload to 64 MiB** (from 32). Under a freeze only one blocked read
  is needed, but 64 MiB guarantees scrub issues device reads through dm-delay
  before the 5s gate rather than satisfying early reads from cache. Safe on
  capacity (2x 512 MiB disks, RAID1, ~400 MiB usable).
- Replace the hand-rolled poll loop with `machine.wait_until_succeeds(... 'Status:
  running' ..., timeout=30)`. 30s is 6x the ~5s surfacing latency and bounds the
  worst-case freeze hold comfortably under 120s.
- **Cleanup reordered: deactivate the throttle FIRST**, then cancel, then wait.
  dm-delay's `delay_presuspend` calls `flush_delayed_bios(dc, true)`
  (`reference/linux/drivers/md/dm-delay.c`), so `dmsetup suspend` (inside
  `dm_delay_deactivate`) immediately drains the blocked read -- the scrub kthread
  leaves D-state and the cancel ioctl can't block on it. Once drained, the 64 MiB
  scrub may auto-finish before `cancel` lands (cancel then returns ENOTCONN), so
  tolerate that (`|| true`) and accept `finished` in the terminal-state wait.
- No `try/finally`: match the straight-line style of the sibling subtests; VM
  teardown on assertion failure releases the in-VM dm-delay state anyway.

### Replacement for the subtest body (`tests/cli/braid-idle.py`, the
`"...genuinely running"` block)

```python
with subtest("braid idle reports busy while a scrub is genuinely running"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/data bs=1M count=64 conv=fsync")
    machine.succeed("sync")
    # Freeze scrub reads (10-minute read delay, released in cleanup) so the scrub
    # cannot reach a terminal state before we sample it. `btrfs scrub status`
    # only prints a `Status:` line once the scrub daemon surfaces a progress
    # record with t_start, which happens on its ~5s cycle (scrub_progress_cycle
    # poll(...,5*1000), reference/btrfs-progs/cmds/scrub.c) -- so the wait below
    # is mandatory, and freezing makes the post-wait sample race-free instead of
    # a timing margin. Status/ioctl queries and `braid idle` do not read the
    # frozen data path, so they stay fast.
    dm_delay_activate(machine, ["disk1", "disk2"], read_delay_ms=600000)
    machine.succeed("btrfs scrub start /mnt/storage > /dev/null 2>&1")
    machine.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+running'",
        timeout=30,
    )

    status, output = machine.execute("braid idle")
    output = output.strip()
    assert status == 1, f"running scrub must make braid idle exit 1, got {status}: {output}"
    assert output.startswith("busy: scrub running"), (
        f"expected 'busy: scrub running', got: {output}"
    )

    # Release the read freeze FIRST: dmsetup suspend triggers dm-delay's
    # delay_presuspend -> flush_delayed_bios(dc, true), which drains the blocked
    # scrub read immediately, so the scrub kthread leaves D-state and the cancel
    # ioctl cannot block on it. Once drained the 64 MiB scrub may auto-finish
    # before cancel lands (cancel then returns ENOTCONN), so tolerate that and
    # accept "finished" as a terminal state.
    dm_delay_deactivate(machine, ["disk1", "disk2"])
    machine.succeed("btrfs scrub cancel /mnt/storage || true")
    machine.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+(aborted|interrupted|finished)'",
        timeout=30,
    )
```

### Preamble / wording updates (mechanism changed from "throttle" to "freeze")

- `tests/cli/braid-idle.py` Scenario line: change
  `... exit 1 during a live scrub held running with dm-delay read throttling.`
  -> `... exit 1 during a live scrub whose reads are frozen via dm-delay so it
  cannot finish before the assertion.`
- `tests/cli/braid-idle.nix` Scenario lines: change
  `Hold a live scrub running with dm-delay read throttling and check for busy
  deterministically.` -> `Freeze a live scrub's reads with dm-delay so it cannot
  finish, then check for busy deterministically.`
- The old inline comments ("Throttle scrub reads...", "Cancel while the throttle
  still holds...") are replaced by the new comments in the code block above.

## Files to modify

- `tests/cli/braid-idle.py` -- subtest body + preamble Scenario line + inline
  comments (the only substantive change).
- `tests/cli/braid-idle.nix` -- preamble Scenario lines only.

Reuse (no changes): `dm_delay_activate` / `dm_delay_deactivate`
(`tests/module/dm_delay_helpers.py`), `machine.wait_until_succeeds`. No Rust
changes -- `cli/src/idle.rs` logic is unchanged and already correct.

## Out of scope (recommended follow-ups, flag only)

These share the same "read_delay to observe a running scrub" fingerprint and
likely the same latent flake; not touched by this plan:

- `tests/module/scrub-lifecycle.py` -- the `resume` and `concurrency` nodes use
  `read_delay_ms=500` + the same wait-for-`Status: running` then race
  `braid lock` / `btrfs scrub cancel`.
- `tests/progress-monitoring.py` -- `read_delay_ms=50` relying on a
  comment-estimated "~10s+" scrub window to capture fixtures (narrowest margin of
  the lot).

(Balance-based `read_delay_ms` tests -- `ups-lb-during-remove.py`,
`braid-add-during-balance.py`, etc. -- are **not** affected: balance surfaces
`Status: running` via the sysfs/ioctl exclop path, not the 5s scrub daemon.)

## Verification

1. Run the single check: `just test-vm braid-idle -v`. Expect the
   "...genuinely running" subtest to pass (`braid idle` exit 1 / `busy: scrub
   running`) and cleanup to reach a terminal scrub state.
2. Confirm it is no longer flaky. nix caches a passing check, so force
   re-execution several times: `just test-vm braid-idle -rebuild` (repeat ~5-10x);
   every run should pass. Optionally check the run log to confirm `Status:
   running` surfaces ~5s after `scrub start` and the freeze is released in
   cleanup.
3. Sanity: `just test-rust` stays green (no Rust changes; the logic is pinned by
   `cli/src/idle.rs#busy_when_scrub_running` and `#busy_unknown_on_scrub_state_unknown`).

## Implementation notes

- The full `read_delay_ms=600000` freeze from the draft plan made `btrfs scrub status`
  time out instead of surfacing `Status: running`: the progress thread must complete
  `BTRFS_IOC_SCRUB_PROGRESS` before it can answer status clients, and that ioctl did
  not complete while the scrub was blocked behind the frozen read. The implementation
  therefore keeps a finite dm-delay read throttle (`read_delay_ms=1000`) plus the
  larger 64 MiB payload, which keeps status and `braid idle` responsive while widening
  the post-gate running window.
- `tests/cli/braid-idle.nix` stayed unchanged because the implemented mechanism is
  still dm-delay read throttling, so its existing Scenario wording remains accurate.

## Follow Up

- `tests/module/scrub-lifecycle.py`: the `resume` and `concurrency` nodes use
  `read_delay_ms=500` plus the same wait-for-`Status: running` pattern before racing
  `braid lock` or `btrfs scrub cancel`; evaluate the same finite-throttle and larger
  payload treatment there.
- `tests/progress-monitoring.py`: the `read_delay_ms=50` fixture capture relies on a
  comment-estimated "~10s+" scrub window; evaluate whether it needs the same scrub
  window widening.
