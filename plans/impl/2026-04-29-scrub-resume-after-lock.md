# Resume scrub after lock or shutdown

## Status

Implemented and staged.

This plan consolidates the scrub-resume work from:

- `plans/wip/scrub-resume-on-shutdown.md`
- `plans/wip/plan-this-fix-to-giggly-frost.md`
- `plans/wip/turn-this-into-a-snug-chipmunk.md`

The final implementation is the single-runner design. The earlier
two-service and shared-`flock` designs are superseded.

## Context

Before this change, `braid-scrub.service` ran:

```sh
btrfs scrub start -B <mount>
```

When the pool went offline, `braid lock` or shutdown stopped the scrub
service and cancelled the in-flight scrub. The next scheduled scrub
started from the beginning. That was wasteful for large arrays because
scrub can take many hours.

btrfs does not provide a true pause command, but btrfs-progs persists
foreground scrub progress in `/var/lib/btrfs/scrub.status.<fsid>`.
`btrfs scrub resume -B <mount>` can continue from that saved progress.

The goal is:

- Cancel in-flight scrub cleanly before lock, sleep, or shutdown.
- Resume saved scrub progress on the next pool-online activation.
- Keep fresh scheduled scrubs on the existing timer cadence.
- Avoid adding a public `braid scrub` command.
- Show cancelled and interrupted scrub states truthfully in status output.

## Reference behavior

The behavior this change relies on was checked against local reference
source, not guessed from man pages.

`reference/btrfs-progs/cmds/scrub.c`:

- Aggregate status strings are `running`, `finished`, `aborted`, and
  `interrupted`.
- A scrub is resumable when the saved record is cancelled or not finished.
- `btrfs scrub resume -B` exits 2 when there is nothing to resume.
- `btrfs scrub start -B` and `resume -B` exit 3 when scrub completes with
  uncorrectable errors.
- `btrfs scrub cancel` exits 2 for "not running".
- The foreground scrub process periodically writes the saved status file, and
  cancellation lets it write a terminal `aborted` state.

`reference/systemd`:

- `Type=simple` services are considered started immediately after fork, so
  `After=` cannot serialize two long-running foreground scrub processes.
- Starting an already active or already queued unit coalesces onto the same
  unit activation instead of creating a second instance.
- Timer activation updates the timer stamp when the timer fires; if the target
  service is already active, the active service still absorbs the start.

## Final design

Only `braid-scrub.service` ever runs `btrfs scrub`.

There are two ways to request that one service:

1. The existing monthly `braid-scrub.timer` starts `braid-scrub.service`.
2. A new pool-online trigger starts `braid-scrub.service` only when saved
   scrub progress is resumable.

Because both paths target the same unit, systemd provides the serialization:
there is no second foreground scrub unit and no `/run/braid-scrub.lock`.

### `braid-scrub.service`

The scrub service remains the long-running foreground service:

- `Type=simple`
- `BindsTo=braid-online.service`
- `After=braid-online.service`
- `ConditionPathIsMountPoint=<mount>`
- `Conflicts` / `Before` for shutdown and sleep
- low scheduling priority via `Nice=19` and `IOSchedulingClass=idle`

Its `ExecStart` now runs:

```sh
braid scrub-resume-or-start --mount <mount>
```

That hidden helper:

1. Runs `btrfs scrub resume -B <mount>`.
2. Treats exit 0 as resumed success.
3. Treats exit 3 as resumed with uncorrectable errors and exits 3.
4. On exit 2, falls back to `btrfs scrub start -B <mount>`.
5. Treats start exit 0 as fresh-start success.
6. Treats start exit 3 as fresh-start uncorrectable errors and exits 3.
7. Treats other resume/start failures as service failure.

This means a timer-fired or manually started service always performs useful
scrub work: resume saved progress first, otherwise start fresh.

### `braid-scrub-resume-trigger.service`

The new trigger is short-lived:

- `Type=oneshot`
- `WantedBy=braid-online.service`
- `BindsTo=braid-online.service`
- `After=braid-online.service`
- `ConditionPathIsMountPoint=<mount>`
- `Conflicts=sleep.target`
- `Before=sleep.target`

Its script runs:

```sh
braid scrub-needs-resume --mount <mount>
```

Then:

- Exit 0 means saved progress is resumable, so the trigger runs
  `systemctl start --no-block braid-scrub.service`.
- Exit 1 means no resume is needed, so the trigger exits successfully.
- Exit 2 means parser or command failure, so the trigger fails and systemd
  records the failure.

The trigger cannot start an unscheduled fresh scrub by itself. It only pokes
the shared scrub service when the parser sees an `aborted` or `interrupted`
scrub status.

### Coalescing timer and resume activation

If the pool comes online with both:

- resumable scrub progress, and
- an overdue monthly timer stamp,

then the trigger and timer both request `braid-scrub.service`. Since there is
only one target unit, systemd coalesces the starts into one service run.

That run executes `scrub-resume-or-start`, resumes the saved scrub, and also
satisfies the overdue timer activation. braid deliberately does not run a
second fresh scrub immediately after the resumed one completes.

## Rust CLI changes

### Parser state split

`ScrubState::Completed` is replaced by distinct terminal states:

```rust
ScrubState::Finished { ... }
ScrubState::Aborted { ... }
ScrubState::Interrupted { ... }
```

The aggregate btrfs scrub-status parser maps the upstream status strings
directly. Unknown terminal words remain `ScrubState::Unknown` so parser drift
does not silently collapse into a known state.

The per-device scrub-status parser also recognizes `interrupted`.

### Hidden scrub helpers

`CmdRequest` gains:

- `BtrfsScrubResume`
- `BtrfsScrubStart`

Two hidden commands are wired through clap:

- `braid scrub-resume-or-start --mount <mount>`
- `braid scrub-needs-resume --mount <mount>`

`scrub-resume-or-start` is the service `ExecStart` helper described above.

`scrub-needs-resume` probes `btrfs scrub status --raw` through the typed
parser and returns:

- 0 for `Aborted` or `Interrupted`
- 1 for `Never`, `Finished`, or `Running`
- 2 for `Unknown`, command failure, or parse failure

`braid scrub-cancel --mount <mount>` remains hidden and is updated for the new
state model. It cancels only `Running`; it treats `Never`, `Finished`,
`Aborted`, and `Interrupted` as no-op success; it hard-fails on `Unknown`.

## Systemd and wrapper changes

`modules/braid/storage.nix`:

- Replaces `btrfs scrub start -B` with `braid scrub-resume-or-start`.
- Adds `braid-scrub-resume-trigger.service`.
- Keeps one foreground scrub service.
- Does not add `braid-scrub-resume.service`.
- Does not add `flock`.
- Factors the cancel script into a shared `scrubCancelScript`.
- Keeps the cancel path explicit through `braid scrub-cancel --mount <mount>`.
- Sleeps briefly after a successful cancel so the foreground btrfs process has
  time to persist the final `aborted` scrub-status record before systemd kills
  the service process.

`modules/braid/braid-wrapper.sh` stops scrub-related units before lock in this
order:

1. `braid-scrub.timer`
2. `braid-scrub-resume-trigger.service`
3. `braid-scrub.service`

The timer stops first so it cannot re-trigger the service during lock. The
trigger stops before the service so a freshly fired trigger cannot queue a new
service start after the scrub service is stopped. The scrub service stops last
so its `ExecStop` can cancel any in-flight scrub before unmount.

## User-visible behavior

After `braid lock`, shutdown, or sleep cancels a scrub, the next unlock resumes
the saved scrub progress automatically.

The monthly timer cadence is unchanged. Unlocking a pool after a cleanly
finished scrub does not start a new scrub unless the timer is overdue.

No public scrub command is added. Users still trigger ad-hoc scrubs with:

```sh
systemctl start braid-scrub.service
```

`braid status` now distinguishes terminal scrub states:

```text
Last scrub: Mon Jan  1 00:00:00 2024 (no errors)
Last scrub: Mon Jan  1 00:00:00 2024 cancelled (will resume)
Last scrub: Mon Jan  1 00:00:00 2024 interrupted
Last scrub: never
Last scrub: running (45%)
```

The JSON state for `last_scrub` changes from `completed` to the more precise
states:

- `finished`
- `aborted`
- `interrupted`

This is acceptable because braid is unreleased and does not preserve backwards
compatibility for old JSON consumers.

The TUI uses the same split state model and shows cancelled/interrupted scrub
status distinctly.

## Documentation updates

Updated documentation:

- `docs/decisions/018-systemd-lifecycle.md` documents the single-runner scrub
  topology, the trigger, the wrapper stop ordering, and why serialization comes
  from targeting one unit rather than from `After=` or `flock`.
- `docs/decisions/020-ups-integration.md` now says scrub is cancelled on
  shutdown and resumed on next pool activation; balance remains out of scope.
- `manual/commands/status.md` documents the new human and JSON scrub states.
- `manual/guides/troubleshooting.md` says braid resumes cancelled scrub work on
  the next pool-online activation without exposing the internal trigger unit
  name.

## Tests

Rust unit tests cover:

- Aggregate scrub parser mapping for `finished`, `aborted`, `interrupted`, and
  unknown status words.
- Per-device `interrupted` mapping.
- `scrub-cancel` no-op behavior for all terminal non-running states.
- `scrub-needs-resume` exit classification for resumable, non-resumable, and
  unknown/error states.
- `scrub-resume-or-start` exit-code handling for resume success, resume
  uncorrectable errors, nothing-to-resume fallback, fresh-start success, fresh
  uncorrectable errors, and real failures.
- `status` human and JSON output for `finished`, `aborted`, and `interrupted`.
- Existing idle and golden parser tests updated from `Completed` to `Finished`.

VM/config tests cover:

- `auto-scrub` asserts the scrub service uses `scrub-resume-or-start`.
- `auto-scrub` asserts the old `braid-scrub-resume.service` is absent.
- `auto-scrub` asserts the new trigger exists only when auto-scrub is enabled.
- `auto-scrub` checks trigger type, lifecycle bindings, mount condition,
  sleep ordering, and generated script contents.
- `scrub-lifecycle` still verifies persistent timer catch-up.
- `scrub-lifecycle` still verifies lock-time cancellation succeeds.
- `scrub-lifecycle` verifies real btrfs scrub cancellation leaves an `aborted`
  state and that the trigger resumes it.
- `scrub-lifecycle` verifies the trigger no-ops when nothing is resumable.
- `scrub-lifecycle` verifies fresh timer stamps do not start new scrubs and
  aged timer stamps do.
- `scrub-lifecycle` verifies an overdue timer and resumable pool-online state
  coalesce into one `braid-scrub.service` run, ending with `Scrub resumed:` and
  no `Scrub is already running` journal entry.

## Verification

Run:

```sh
just test-rust
just test-vm auto-scrub
just test-vm scrub-lifecycle
just test-vm
```

For parser compatibility after any parser-critical toolchain bump, also run the
standard parser fixture workflow documented in `AGENTS.md`.

## Rejected intermediate designs

### Two long-running scrub services

The first design added a foreground `braid-scrub-resume.service` for
pool-online resume and kept `braid-scrub.service` for timer/manual scrubs.

That made the activation semantics clear, but it introduced two units that
could both run `btrfs scrub` against the same filesystem.

### Shared `flock`

The second design serialized those two services with
`flock -F -x /run/braid-scrub.lock`.

That would have fixed the immediate race, but made an internal lock the
load-bearing correctness mechanism. It also added more moving parts to tests
and lifecycle docs.

The final design is simpler: one foreground service, one short trigger, and
systemd coalescing on a single target unit.

## Out of scope

- Public `braid scrub` command.
- True scrub pause support. btrfs does not provide that primitive.
- Balance pause/resume. UPS and shutdown balance behavior remains separate.
- Compatibility shims for the old JSON `last_scrub.state = "completed"` value.
