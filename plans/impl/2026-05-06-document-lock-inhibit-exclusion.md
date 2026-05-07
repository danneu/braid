# Plan: document why `braid lock` is excluded from the sleep-inhibitor scope

## Context

`docs/decisions/019-inhibit-sleep.md` enumerates the four mutating commands
that hold a logind sleep inhibitor (`add`, `remove`, `remove-missing`,
`replace`) plus `recover`'s replay path. `braid lock` is the only
remaining pool-state mutator that is **not** covered, and the doc never
says so. A reviewer recently flagged the omission and asked whether
`braid lock` should acquire the inhibitor too.

Verification (`/verify-issue` round) found:

- `cli/src/lock.rs` (`cmd_lock` -> `cmd_lock_impl` -> `LockPlan::execute`)
  contains no `SleepInhibitor::acquire` call.
- `cli/src/main.rs:331` constructs one `RealSleepInhibitor` and threads it
  into the four mutators + `recover` only -- not into `cmd_lock`.
- `modules/braid/storage.nix`'s `braid-online.service` has
  `TimeoutStopSec = 5min`, `ExecStop = ... braid lock`, and no
  `Conflicts = sleep.target` -- by design, the pool stays mounted across
  suspend (per `016-auto-suspend.md`).
- `cli/src/mapper_close.rs:6-7` caps each mapper close at
  `(3 - 1) * 500ms = 1s` of retry sleep; the lock pipeline itself is
  short and idempotent.

The omission is intentional and defensible -- a half-completed lock
(umount done, some mappers still open) is recovered by re-running
`braid lock`, unlike a half-completed `btrfs replace`/balance which can
corrupt topology or restart hours of work. But the *reasoning* is not
written down anywhere, and the next reviewer or agent will hit the same
question. Goal: a short authoritative statement in `019-inhibit-sleep.md`
so the next person reads it instead of re-deriving it.

This is doc-only. The reviewer's proposed rationale ("deadlock risk in
the systemd-stop path") is **not** the actual reason and should not be
copied verbatim -- there is no specific deadlock mechanism that adding a
`systemd-inhibit` subprocess to `ExecStop=braid lock` would introduce.
The actual reasons are recoverability, suspend-irrelevance during
shutdown, and the design that already keeps the pool mounted across
suspend.

## Files

- `docs/decisions/019-inhibit-sleep.md` -- add a `### Excluded: braid
  lock` subsection under `## Current application`, immediately after the
  `braid recover` paragraph and before `## Consequences`.

No code, test, or NixOS module changes.

## Edit

Insert a new subsection after the existing `braid recover` paragraph
(currently the last content under `## Current application`). Proposed
body, ASCII-only and `--`-not-em-dash per project style:

```markdown
### Excluded: `braid lock`

`braid lock` deliberately does not acquire the sleep inhibitor, even
though its mutation window (umount + per-mapper `cryptsetup close`) is
non-trivial in wall-clock time. This is the worked example of the
deciding question below applied to lock work specifically:

- **Recoverability.** A lock interrupted mid-flight leaves a state that
  re-running `braid lock` advances on, to the extent its existing
  probes can detect. Specifically:
  - `plan_lock`'s `mountpoint -q` skips the umount step when the pool
    is already unmounted (`cli/src/lock.rs`'s `plan_lock`).
  - The per-mapper close path checks `fs.exists("/dev/mapper/<name>")`
    before issuing `cryptsetup close` and reports "already closed"
    otherwise, so closed membership mappers do not re-error on a
    follow-up run.
  - Orphan mappers (`braid-*` paths not in `pool.json`) are re-scanned
    on each invocation and closed; close failures still surface as
    fatal errors, and a `/dev/mapper` scan failure is warned and
    yields an empty orphan list for that run -- not silently swallowed.

  Unlike `replace`/`add`/`remove`/`remove-missing`, there is no
  kernel-level topology corruption window and no hours-long restart
  cost. The point is that a partially-completed lock does not poison
  subsequent invocations -- not that every failure is hidden.
- **Shutdown-driven `ExecStop`.** When `braid lock` runs as
  `braid-online.service`'s `ExecStop=` during system shutdown, the
  system is heading to `shutdown.target`/power-off, not to suspend. A
  sleep inhibitor acquired during that window is redundant -- logind
  does not schedule a suspend transition mid-shutdown.
- **Manual stop and user-lock reentry.** `ExecStop=braid lock` also
  fires on a manual `systemctl stop braid-online.service` and on the
  wrapper's post-lock `systemctl stop braid-online.service` for
  user-initiated `braid lock` (see
  `docs/decisions/018-systemd-lifecycle.md:131` and
  `modules/braid/storage.nix`'s `braid-online` definition). Those
  paths do not enjoy the shutdown-driven guarantee above; their
  justification is the recoverability + short-duration argument, not
  the shutdown-target one.
- **Suspend context.** `braid-online.service` has no
  `Conflicts = sleep.target` (see `modules/braid/storage.nix`). By the
  `016-auto-suspend.md` design the pool stays mounted across suspend,
  so the only realistic mid-lock-suspend race is a user-initiated
  `braid lock` colliding with autosuspend's idle countdown. That
  window is narrow (lock is short) and the failure mode is
  recoverable, per the first bullet.
- **`ExecStop` budget.** `braid-online.service` runs lock under
  `TimeoutStopSec = 5min`. Adding subprocess work to that path (a
  `systemd-inhibit` fork plus its supervised `sh + sleep` child) buys
  no protection commensurate with the added shutdown-path complexity.

If a future change makes lock's mutation window genuinely long
(e.g. a multi-minute pre-lock balance), revisit this exclusion under the
same deciding question.
```

Wording must follow the project's CLI/doc style: ASCII `--` not em-dash,
straight quotes, plain ASCII throughout. The surrounding doc uses em-dash
in a few places (`### braid replace` "long-running phase that the
inhibitor primarily protects -- the inhibitor primarily ..."); match the
new subsection to the existing local style of the file. If the file
already has em-dashes, mirror them; if not, stay ASCII. (Spot-check on
read: 019 currently uses both `--` and `—`. The new subsection uses
`--` consistently, matching the project-wide CLI Output Style rule in
`AGENTS.md`.)

## What this plan deliberately does NOT change

- No edit to `cli/src/lock.rs`. A "see 019" comment at the top of
  `lock.rs` was considered and rejected: the doc-decision is the
  canonical place, the omission is one negative space among many, and
  the project's doc-comment rule (`AGENTS.md`) targets new top-level
  items, not historical absences of imports.
- No edit to `cli/src/main.rs`. The single `RealSleepInhibitor`
  construction + threading already encodes the policy mechanically.
- No new tests. There is no behavior change to lock; the assertion the
  doc makes about lock's recoverability is already covered by existing
  unit tests (`lock_partial_state`, `lock_already_locked`,
  `lock_continues_closing_after_mapper_error`,
  `lock_orphan_close_failure_is_fatal`, etc. in `cli/src/lock.rs`'s
  `tests` module).
- No change to the closing "deciding question" paragraph -- the new
  subsection acts as the worked example for that rule.

## Verification

- Render the file and re-read the new subsection in context. Confirm it
  flows from the `### braid add` -> recover paragraph -> new subsection
  -> `## Consequences` ordering and reads as one continuous policy
  document.
- `rg "braid lock" docs/decisions/019-inhibit-sleep.md` returns the new
  subsection.
- `rg "—" docs/decisions/019-inhibit-sleep.md` -- audit em-dash
  occurrences in the file as a whole. If the new subsection introduces
  any, replace with `--` per CLI Output Style.
- No code/test runs needed; doc-only.
- Optional: `git diff docs/decisions/019-inhibit-sleep.md` and confirm
  the only change is the inserted subsection.

## Out-of-scope follow-ups (do not do here)

- Whether `braid lock` should additionally acquire the wrapper-side
  pool lock (`/run/braid-pool.lock`). The wrapper currently does not
  acquire it for `lock` (`modules/braid/braid-wrapper.sh:51-78`); that
  is a separate decision orthogonal to the inhibitor question and would
  require its own ADR.
- Whether the per-mapper close retry budget (`mapper_close.rs:6-7`)
  should grow or shrink. Independent of the inhibitor scope.
