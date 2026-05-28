# Plan: document monitor's silent exit-0 on pool-lock contention

## Context

`braid monitor` uses `LockPolicy::MonitorSilent`, which maps
`PoolLockError::AlreadyHeld` to `std::process::exit(0)` so a contended
timer cycle does not start alert notification
(`cli/src/main.rs:1024-1031`). This behavior is intentional and is
documented in three places already:

- `docs/design/principles.md:69` -- "Timer-driven monitoring may exit 0
  silently on contention because a missed cycle is harmless and exit 1
  would falsely start alert notification."
- `docs/design/decisions/018-systemd-lifecycle.md:98` -- "Exit 0 is
  reserved for healthy, pool-offline, and pool-lock-contended cycles".
- `docs/design/decisions/018-systemd-lifecycle.md:162` -- "monitor exits
  0 silently on contention so a skipped timer cycle does not start
  alert notification."

Four operator-facing surfaces are out of sync with that contract and
miss the contention case. The full inventory was confirmed with `rg`
across `docs/`, `cli/src/`, and the guide tree:

- `docs/commands/monitor.md:23-30` -- the exit-code table treats exit 0
  as "Healthy, or pool is offline" only.
- `docs/design/decisions/014-alerts.md:82-85` -- the exit-code list says
  "0 -- ok or pool offline with no active alerts" only. ADR 018 links
  back to ADR 014 as the cause taxonomy authority, so 014 should match.
- `docs/guides/monitoring-and-alerts.md:155` -- the ASCII pipeline
  diagram summarizes `braid monitor (exit 0 = ok, 1 = alert, 2 = setup
  error)` without naming contention.
- `cli/src/main.rs:72` -- the Clap `Monitor` doc-comment (rendered as
  `braid monitor --help`) reads `exit 0 = ok/offline, exit 1 = alert
  (...), exit 2 = setup error (...)` without naming contention.

Symptom this prompted: an operator debugging "monitor exited 0 but I
have a degraded pool" has no documented explanation that a concurrent
`add`/`remove`/`lock`/`ack` causes a clean skip. Recent commit
`bc6ef90 docs(ack): document pool-lock contention` patched `ack.md` for
the same reason; monitor was overlooked because its behavior is a silent
exit-0 skip rather than a refusal.

This plan is documentation/help-text only. The one source edit is to a
Clap doc-comment string; it changes no behavior.

## Changes

### 1. `docs/commands/monitor.md` -- expand the exit-0 row

Update the exit-code table at lines 25-29 so the exit-0 row also names
the lock-contention case. The three exit-0 conditions (healthy, pool
offline, lock contention) all mean "nothing to act on", so keeping them
under one row is appropriate; do not split into separate rows.

Concrete edit:

```
| **0** | Healthy, pool is offline, or another braid command holds the pool lock (cycle skipped, re-evaluated on the next timer tick) |
```

No other section of `monitor.md` needs to change. The "What happens
under the hood" list (lines 42-49) starts at the post-lock step and
does not need a new step 0 -- the exit-codes row carries the operator
information.

### 2. `docs/design/decisions/014-alerts.md` -- expand the exit-0 bullet

Update line 83 of the "`braid monitor` is a pure detector" exit-code
list so it mirrors ADR 018's wording at line 98.

Concrete edit:

```
- **0** -- ok, pool offline with no active alerts, or pool-lock-contended cycle (silently skipped; re-evaluated on the next timer tick)
```

No other ADR 014 content needs changes. Line 89's pool-lock paragraph
already explains that `monitor` shares `/run/braid-pool.lock` with the
other writers, so the contention case is the natural consequence of an
already-documented serialization contract.

### 3. `docs/guides/monitoring-and-alerts.md` -- expand the diagram annotation

Update the ASCII pipeline diagram at line 155 so the exit-0 annotation
matches the new monitor.md row. Keep the diagram compact; the longer
"re-evaluated on the next timer tick" prose lives in the command page
and the ADR.

Concrete edit:

```
    -> braid monitor (exit 0 = ok/offline/lock-contended, 1 = alert, 2 = setup error)
```

No other guide content needs changes. The "When `braid monitor` detects
an issue (exit code 1)" sentence at line 23 is exit-1-specific and is
unaffected.

### 4. `cli/src/main.rs:72` -- expand the Clap `Monitor` help text

Update the doc-comment so `braid monitor --help` matches the new
monitor.md row. Keep it on one line to match the surrounding Clap
help-text style.

Concrete edit:

```
/// Check disk health: exit 0 = ok/offline/lock-contended, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (e.g. pool-lock I/O, config load)
```

This is the one source-file edit in the plan. It changes only a
clap-generated help string; no behavior, no tests, no fixtures.

## Out of scope

- `tests/cli/braid-monitor.py:67` has a slightly vague comment
  ("lock errors also exit 2") that is technically correct only for the
  `PoolLockError::Io` arm; the `AlreadyHeld` arm exits 0. The test
  itself is correct (it pins the config-load exit-2 path), and the
  comment's intent reads correctly in context, so leave it. Touching it
  would expand the diff without changing any verified behavior.
- No new test. Monitor's exit-0-on-contention behavior is already pinned
  by two existing VM tests, and this plan changes no behavior:
  - `tests/module/alert-state-lock.py:202` -- "monitor exits 0 without
    touching alert-latch.json when the pool lock is already held"
    (asserts `systemctl start braid-monitor.service` succeeds and the
    pre-existing corrupt latch is left untouched while a holder owns
    the lock).
  - `tests/module/pool-lock-precedes-state-read.py:151` -- "monitor
    exits silently before broken config" (asserts `with_holder("braid
    --config /nonexistent/braid.json monitor")` returns `rc == 0` with
    empty output).

## Verification

This is a documentation/help-text change. Verify by:

1. Reading the rendered `docs/commands/monitor.md` and confirming the
   exit-code table now names the contention case under exit 0.
2. Reading the rendered `docs/design/decisions/014-alerts.md` and
   confirming the exit-code list matches ADR 018's exit-0 wording.
3. Reading the rendered `docs/guides/monitoring-and-alerts.md` and
   confirming the pipeline diagram annotation matches.
4. Running `cargo build -p braid-cli` and then `braid monitor --help`
   (or inspecting the post-edit `cli/src/main.rs:72`) to confirm the
   Clap help text matches.
5. Building the mdBook to confirm no broken cross-links:
   `mdbook build docs` (configured in `docs/book.toml`).
6. Confirming the change matches the code by re-reading
   `cli/src/main.rs:1024-1031`
   (`LockPolicy::MonitorSilent` -> `Err(PoolLockError::AlreadyHeld) =>
   std::process::exit(0)`).

No Rust tests, VM tests, or fixture refreshes are required -- nothing
in the parser-compatibility surface or systemd lifecycle is touched,
and the existing tests cited above already pin the behavior.
