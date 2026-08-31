# Auto-scrub skips busy pools and retries until clear

## Context

`braid-scrub.timer` -> `braid-scrub.service` starts a scheduled scrub
unconditionally. Observed on caja (2026-08-31): a `braid add` RAID1 convert
balance was mid-flight with the monthly scrub due at midnight; nothing would
have stopped the scrub from piling onto the same spindles. Worse, a scheduled
scrub firing during a `btrfs replace` is kernel-rejected, exits 1, and
spuriously fires the `braid-scrub-failed` alert path.

ADR 018 currently records "a balance can overlap a running scrub" as the
design position; this plan revises it for the *starting* scheduled scrub (an
already-running scrub still coexists with a later balance).

## Decision

Gate the scheduled scrub in the CLI, per ADR 018's
Rust-dispatch-as-synchronization-layer principle: `braid
scrub-resume-or-start` checks, before any scrub command and before the
cancel-marker cleanup, for

- the pool lock (`/run/braid-pool.lock`): the gate acquires it non-blocking
  (`pool_lock.rs#RealPoolLock::acquire`) and a lock held by another braid
  process is itself a skip reason. This is the ordering boundary against braid
  mutators, which hold the lock across their whole run -- including the LUKS
  work that precedes any btrfs exclusive operation, a window the sysfs check
  alone cannot see,
- any btrfs exclusive operation (balance running or paused, device
  add/remove/replace, resize, swap activate), via the existing sysfs primitive
  `cli/src/preflight.rs#check_any_btrfs_exclusive_op`, and
- an interrupted-operation journal (`pending-op.json`, via
  `preflight.rs#check_no_pending_operation`); an unreadable journal counts as
  present (same "run braid recover" condition).

The lock is held from the first check until the kernel has accepted the
scrub -- not merely until the child is spawned. `btrfs scrub` is outside the
kernel exclusive-operation set (`cli/src/idle.rs`), so the window between
`fork` and `scrub.c#scrub_start`'s ioctl covers mount open, fs/device queries
and status-file work: unbounded, not negligible. So the scrub dispatch splits
spawn from wait (`cli/src/cmd.rs`, today one blocking `Command::output()`),
and the gate holds the lock across the spawn plus a bounded confirm poll of
the existing `CmdRequest::BtrfsScrubStatus` primitive. The parent then waits
on the *same* child, so the authoritative exit code (I5) is unchanged.

The lock is released on a *terminal* outcome, not merely on child exit: a
confirmed `ScrubState::Running`, or an exit that ends the run. `btrfs scrub
resume -B` exit 2 ends no run -- it is the fallback into `start -B` -- so the
same guard is carried through the fresh spawn and its confirm poll rather than
released and re-taken, which would leave the fallback start ungated.

Holding the lock for the scrub's full multi-hour run is not an option: it
would block every mutation, contradicting ADR 018's position that a balance
may overlap an *already-running* scrub. Dispatch therefore keeps
`LockPolicy::None` for `ScrubResumeOrStart` (`cli/src/main.rs#lock_policy`) --
that policy holds its guard for the whole command -- and the command owns the
scoped acquire itself.

Any of the three tripping makes the run a **skip**: no scrub starts, no side effects on
the cancel-marker path, one ASCII journal line names the reason, and the
process exits with a new dedicated code **4**.

Retry-until-clear via systemd, no new units: `braid-scrub.service` gets
`SuccessExitStatus = [3 4]`, `RestartForceExitStatus = [4]`,
`RestartSec = cfg.autoScrub.retryInterval` (new module option, systemd time
span, default `1h`), `StartLimitIntervalSec = 0`. Genuine failures (exit 1)
keep `Restart=no` semantics: fail once, alert once.

Reboot gap: a skip writes a durable deferred flag (new `StatePaths` accessor
under `/var/lib/braid`), cleared when a real scrub run begins.
`braid scrub-needs-resume` reports `Yes` when the flag exists, so the existing
pool-online resume trigger re-pokes the gated service after boot/unlock with
its script unchanged.

Flag I/O is fail-closed, not best-effort: exit 4 is returned only once the
deferral is durable on disk. A creation or clearing failure is a genuine
failure (exit 1, alert) -- the same discipline
`scrub_resume_or_start.rs#clear_stale_cancel_marker` already applies, and for
the same reason: a best-effort write that silently fails strands the scrub
until the next calendar firing, and a best-effort clear re-starts a fresh
scrub on every later pool-online. Presence is a metadata existence check --
the flag's contents are never read -- and only `NotFound` means absent: any
other inspection error is a hard failure too, never "no deferral pending".

No UPS gate (user decision).

## Invariants

- I1: A scheduled scrub never starts while another braid mutation holds the
  pool lock, while any btrfs exclusive operation is in flight (running or
  paused), or while `pending-op.json` exists (or is unreadable). The gate
  holds the pool lock from the first check until the kernel has accepted the
  scrub, so no braid mutation can interleave between the gate and the scrub's
  kernel registration.
- I2: A skip is not a failure: `braid-scrub-failed.service` (flag + alert)
  never fires on a skipped run, including when the retry wait is stopped by
  `systemctl stop` or `sleep.target`.
- I3: A skip defers the scrub by at most `retryInterval` past clearance while
  the system stays up, and at most until the next pool-online after a reboot
  -- never silently until the next calendar firing. A skip is only reported
  (exit 4) once the deferred flag is durably on disk. In a `braid-scrub.service`
  run, a flag that cannot be written, cleared, or inspected is a hard failure
  (exit 1, alert), never a silent skip or an ungated start. On the
  resume-trigger path, `braid scrub-needs-resume` keeps its own exit contract
  (0 Yes / 1 No / 2 error), so an inspection failure surfaces as its existing
  exit-2 failed unit -- journalled, not alerted, since that unit has no
  `onFailure`. Neither path may report "no deferral pending".
- I4: Gate classification is asymmetric on purpose: busy/pending states skip,
  but unreadable or unrecognized sysfs exclusive-op state is a hard failure
  (exit 1, alert). Mapping probe breakage to skip would starve scrubs forever
  with no alert.
- I5: Existing exit-code contract is preserved: 0 clean, 3
  completed-with-uncorrectable (alert-silent per ADR 014 path), 1 genuine
  failure -> alert; the deliberate-cancel (ExecStop marker) path still exits 0.
- I6: A skip leaves the cancel marker and scrub state untouched (gate runs
  before `clear_stale_cancel_marker`; systemd does not run `ExecStop` when the
  main process exits on its own).

## Proof obligations

- PO1 (I1, I6): Rust unit tests -- with a paused balance under the sysfs seam,
  with `pending-op.json` present, with an unreadable and with a malformed
  `pending-op.json`, and with the pool lock held by another process --
  `cmd_scrub_resume_or_start` skips with zero runner requests, and a
  pre-existing cancel marker is left untouched by the skip. End-to-end, a VM
  test with a real paused balance shows the service exiting 4 without starting
  a scrub.
- PO2 (I2): VM assertion that after a skip (and after a stop during the retry
  wait) `braid-scrub-failed` never activated and `/var/lib/braid/scrub-failed`
  is absent; scrub-alert's exit-code-parameterized fake scrub gains an exit-4
  case asserting no alert, while the existing exit-1 -> alert case still
  passes.
- PO3 (I3): VM test with `retryInterval` overridden to seconds: skip while
  balanced-busy, then after the balance finishes a real scrub run occurs
  without another timer firing; separately, deferred flag present at
  pool-online makes the resume trigger start the service.
- PO4 (I4): Rust unit test: unreadable/unrecognized exclusive-op sysfs ->
  `Err`, not skip.
- PO5 (I5, I6): existing Rust unit tests and scrub-alert/scrub-lifecycle VM
  tests stay green; gate-clear path behaves identically to today, and a stale
  deferred flag is cleared when a real run begins.
- PO8 (I1, I5): Rust unit tests over the split spawn/wait seam -- the pool
  lock is still held while the confirm poll reports a not-yet-running scrub;
  is released once the poll reports `ScrubState::Running` (so a concurrent
  mutation can acquire it during the scrub); is held continuously across the
  resume-exit-2 fallback, so the fresh `start` is spawned and confirmed under
  the same guard; is released on a terminal exit that starts no scrub; is
  released on confirmation failure while the parent keeps waiting on the same
  child; is released early when the confirm poll observes a `Running` scrub
  that predates this child (a scrub already in flight), whose non-zero exit
  still classifies as today; and in every case the result is classified from
  that child's exit code as today.
- PO6 (unit contract): `tests/module/auto-scrub` asserts the new directives on
  `braid-scrub.service` (`SuccessExitStatus` incl. 3 and 4,
  `RestartForceExitStatus=4`, `RestartSec` = configured retryInterval,
  `StartLimitIntervalSec=0`) and the resume-trigger unit topology.
- PO7 (I3): Rust unit tests over the `StatePaths`/`Filesystem` seam -- a
  failing deferred-flag write returns `Err` (exit 1) rather than a skip, and a
  failing clear on a gate-clear run returns `Err` rather than starting the
  scrub, and a flag-inspection error other than `NotFound` returns `Err`
  rather than reporting no deferral pending.

## Non-goals

- Cancelling or deferring a scrub when a mutation starts *after* the scrub
  began.
- Surfacing "scrub skipped" in `braid status` / `doctor`.
- UPS-on-battery gating of the scrub.

## Accepted risks

- AR1: A *manual* suspend during the retry wait loses the retry until the next
  pool-online or calendar firing. Autosuspend cannot cause this: `braid idle`
  already reports Busy during exclusive operations.
- AR2: If the confirm poll itself cannot classify -- deadline expiry, or an
  unreadable/unrecognized `btrfs scrub status` -- the gate releases the lock
  and continues waiting on the child, logging one ASCII line naming the
  unconfirmed start. The child may then still be pre-ioctl for an unbounded
  time, so a mutation acquiring the lock in that window can overlap the scrub
  exactly as before this plan. The alternative, holding the lock until the
  child exits, would block every mutation for hours on a
  parser-compatibility break; the residual is journalled rather than silent,
  and PO8 pins the release.

## Implementation discretion

- Exact skip-line wording (ASCII only), result-variant/flag naming, and how
  the `Filesystem` + `StatePaths` seams thread into the two scrub command
  signatures.
- Confirm-poll cadence and deadline (any value comfortably above scrub
  startup and well below the retry interval leaves behavior unchanged), and
  the shape of the spawn/wait seam on `CommandRunner`.

## Critical files

- `cli/src/scrub_resume_or_start.rs`, `cli/src/scrub_needs_resume.rs`,
  `cli/src/main.rs` (exit-4 arm; pass `RealFilesystem` as `Commands::Monitor`
  already does), `cli/src/state_paths.rs`, `cli/src/pool_lock.rs`,
  `cli/src/cmd.rs` (split the scrub dispatch's spawn from its wait)
- `modules/braid/storage.nix`, `modules/braid/options.nix` (retryInterval)
- Tests: `tests/module/auto-scrub.{nix,py}`, `tests/module/scrub-alert.{nix,py}`,
  `tests/module/scrub-lifecycle.{nix,py}` or a new `scrub-skip-retry` check
  registered in `flake.nix`; `tests/module/balance_helpers.py` provides
  `pause_balance_with_remaining_work`
- Docs: `docs/design/decisions/018-systemd-lifecycle.md` (revise
  `#pool-lock-mutual-exclusion` and the scrub-lifecycle section),
  `docs/internals/tool-behavior/scrub-failure-alerts.md` (exit-code table +
  fixed false-alert-during-replace), `docs/guides/nixos-configuration.md`
  (option table: `braid.autoScrub.retryInterval`),
  `docs/guides/day-to-day-nas-usage.md` (scrub guidance: a scrub due during a
  braid operation is skipped and retried, not failed), ADR 033 if it
  inventories braid-scrub directives

## Verification

TDD: write the Rust unit tests and VM assertions first, confirm they fail for
the right reason, then implement.

```
cargo test --manifest-path cli/Cargo.toml
nix build .#checks.x86_64-linux.braid-auto-scrub
nix build .#checks.x86_64-linux.scrub-alert
nix build .#checks.x86_64-linux.scrub-lifecycle   # or scrub-skip-retry
nix flake check
```

## Commit progress

- [x] 1. refactor(cli): split scrub process spawning from completion
- [x] 2. feat(scrub): defer busy scheduled scrubs and retry

## Implementation notes

- Spawn/wait seam shape (commit 1, plan discretion): `CommandRunner` gains a
  defaulted `spawn` returning `Box<dyn PendingCommand>`, whose `wait(self:
  Box<Self>)` consumes the handle. The default implementation runs the request
  eagerly and replays the outcome from `wait`, so the ~40 existing
  `CommandRunner` test doubles keep compiling unchanged and none of them needs a
  second execution path; only `RealRunner` overrides it with a true
  `Command::spawn`. The request is still logged at `spawn` time in `MockRunner`,
  so the "zero runner requests on a skip" assertions PO1 calls for stay
  meaningful.
- `RealRunner::exec` is now `spawn_exec(..)?.wait()` rather than a parallel
  `Command::output()` call, and the preview-only / requires-stdin guards moved
  into a shared `RealRunner::guard_no_stdin` used by both `run` and `spawn`.
  This keeps stdio setup, child-env policy (ADR 034), and the two safety guards
  from drifting between the blocking and deferred paths -- the alternative,
  duplicating them, is exactly how a second process-creation entry point ends up
  bypassing the preview guard.
- The split introduces one hazard worth naming: stdout/stderr are pipes nobody
  drains until `wait`, so a child that outruns the pipe buffer before `wait` is
  called would block. Documented on the trait method; the scrub is safe because
  btrfs prints its summary at completion and the confirm poll is bounded.
- ADR 034's spawn-site inventory named `RealRunner::exec`, which is no longer
  where `apply_child_env` is called; updated it to `RealRunner::spawn_exec`.

- The confirm poll needs to know the child has exited, not just whether a scrub
  is registered: `btrfs scrub resume -B` with nothing to resume exits in
  milliseconds and never reaches the ioctl, so a poll that only watched for
  `Running` would spin to its deadline and release the pool lock *before* the
  fallback `start` was issued -- leaving the scrub that actually happens
  ungated. `PendingCommand` therefore gained a non-blocking `has_exited`
  alongside commit 1's `wait`. The loop tests status first and exit second, so
  a confirmed-running scrub still releases early while an exited child keeps the
  guard for the fallback. `has_exited` reports `true` when the child's state
  cannot be determined: the response is to stop polling and reap, where the real
  error surfaces.
- Confirm-poll defaults (plan discretion): 250ms cadence, 60s deadline. Bounded
  by two sides -- comfortably above scrub startup, and far below the 1h default
  retry interval. The residual it buys is that AR2's worst case now holds the
  pool lock for up to 60s, so a concurrent fail-fast `braid lock` could be
  refused in that window; that is strictly better than the alternative of
  holding it for the scrub's multi-hour run.
- The deferred flag's write/clear/inspect go through `std::fs` + `StatePaths`
  rather than the `Filesystem` seam, matching `clear_stale_cancel_marker`'s
  existing precedent in this file. The seam's `exists()` returns `bool` and
  coerces I/O errors to `false`, which is exactly the "cannot tell" -> "nothing
  pending" collapse I3 forbids; the tests drive the error paths through
  `StatePaths::custom` under a temp dir instead.
- `preflight::ExclusiveOpError` widened from `pub(crate)` to `pub` because it is
  now carried by `ScrubResumeOrStartError::ExclusiveOpUnknown`, part of a
  command's public error surface. It already wrapped the `pub` `ExclusiveOp`.
- The pool-lock lifetime is observed in unit tests by a `MockRunner` handler
  that tries to take the same lock from a second handle at btrfs-spawn time.
  flock is per-open-file-description, so a second handle in the test process
  sees exactly what a peer braid process would -- this is what makes "held
  across the resume-exit-2 fallback" and "released once confirmed running"
  distinguishable rather than untestable.
- `tests/module/scrub-skip-retry.py` masks `braid-scrub.timer` before the pool
  ever comes online. With `Persistent=true` and no prior stamp the monthly timer
  can fire on activation, which would make a timer-driven run indistinguishable
  from the exit-4 retry and the pool-online resume the test is there to pin.
- The new PO6 directives are asserted from the unit file text rather than
  `systemctl show`: the exit-status list properties render in a
  systemd-internal shape, while the directives themselves are the contract.

