---
intent: Document how a failed scheduled scrub becomes a braid alert, and why a deliberate cancel, a corruption-found scrub, a not-due or already-running poll, and a busy skip do not. Read before changing the scrub unit's onFailure/SuccessExitStatus in modules/braid/storage.nix, the cancel-request marker in modules/braid/storage.nix#scrubCancelScript / cli/src/scrub_resume_or_start.rs, or the ScrubFailed alert source in cli/src/alert.rs.
status: Active
---

# Scrub-failure alerts

Reference for the `ScrubFailed` alert source: how `braid-scrub.service`
failures reach the operator, and why the look-alike non-failures (a deliberate
cancel, a corruption-found scrub, a poll that found the pool fresh or already
being scrubbed, and a scrub skipped because braid was busy with the pool) stay
silent. The lifecycle
authority is [ADR 018](../../design/decisions/018-systemd-lifecycle.md#braid-scrubtimer--scrub-service----lifecycle-bound-scrub-poll);
the alert-model authority is [ADR 014](../../design/decisions/014-alerts.md#two-detection-sources-one-alert-model).

## The btrfs exit codes braid keys off

`braid scrub-resume-or-start` runs `btrfs scrub resume -B` (then `start -B` on
a fallback). The exit codes that matter:

| btrfs exit | Meaning | braid maps to |
|---|---|---|
| 0 | scrub completed cleanly | service success |
| 2 | nothing to resume | fall back to `btrfs scrub start -B` |
| 3 | scrub completed, uncorrectable errors found | service success (`SuccessExitStatus=3`) |
| other (1) | scrub failed to run/complete, **OR** was cancelled, **OR** was refused because a scrub is already running | success iff a cancel was requested or the refusal is btrfs's already-running wording, else failure |

braid's own `scrub-resume-or-start` exit codes, as the unit sees them:

| braid exit | Meaning | service outcome |
|---|---|---|
| 0 | scrub completed cleanly, was deliberately cancelled, was not due, or found a scrub already running | success |
| 3 | scrub completed with uncorrectable errors | success (`SuccessExitStatus=3`) |
| 4 | busy gate skipped the run; no scrub started | success (`SuccessExitStatus=4`); the next poll retries |
| 1 | genuine failure, including an unreadable gate or an unreadable scrub record | failure -> `onFailure` -> alert |

Four of these are deliberately *not* execution failures:

- **Exit 3 (corruption found).** The scrub ran to completion; it simply found
  uncorrectable errors, which btrfs has already written into the per-device
  error counters. Those reach the operator through the monitor's
  `BtrfsDeviceErrors` device-stats poll, so a scrub-status probe would be
  redundant ([ADR 014](../../design/decisions/014-alerts.md#two-detection-sources-one-alert-model)).
  The scrub unit declares `SuccessExitStatus = [ 3 4 ]` so this never reaches
  `onFailure`.
- **Exit 0 on a poll that ran no scrub.** The timer polls hourly, so most runs
  do nothing. `braid-scrub.service` reads btrfs's own scrub record once at
  entry, before taking any lock, and exits 0 with a journal line when the pool
  owes no scrub -- either because the last one is inside the freshness window
  (`scrub not due: last scrub started/resumed ...`) or because a scrub is
  running right now ([ADR 035](../../design/decisions/035-scrub-scheduling-by-freshness.md)).
  Neither is a skip: a skip means "owed but blocked", and these mean "not owed".
- **Exit 4 (busy skip).** braid's gate refused to start a scrub onto a pool that
  is already being worked on: another braid process holds `/run/braid-pool.lock`,
  a btrfs exclusive operation is in flight (running *or* paused), or
  `pending-op.json` exists. No scrub started and nothing was touched, so there
  is nothing to alert on, and nothing to record either -- the next hourly poll
  re-derives the whole decision from btrfs's record. This also fixed the
  spurious alert a scheduled scrub raised when it fired during a `btrfs
  replace`: the kernel rejects that scrub, btrfs exits 1, and the old unit read
  it as a genuine failure.

  The gate has three conditions, not four. "A scrub is already running" used to
  be the fourth, because a hand-run `btrfs scrub` takes no pool lock and scrub
  is not a btrfs exclusive operation, so nothing else could see it. It is now
  the entry classifier's `AlreadyRunning` -- exit 0 above -- because a scrub in
  flight is not a scrub deferred. The entry probe still cannot close the window
  alone: an external scrub can start between it and braid's own spawn, and
  `btrfs scrub resume` then refuses with exit 1 (`scrub.c`'s
  `is_scrub_running_on_fs` guard, which the resume path shares with start). That
  refusal -- recognized solely by the literal `Scrub is already running.` in the
  invocation's own stderr, never by a re-probe, which would be racy in both
  directions -- lands on the same exit 0. The wording is behavior-locked by
  [`tests/repro/btrfs-scrub-start-rejected-during-scrub.py`](../../dev/testing.md#live-tool-behavior-locks).
- **Exit 1 (ambiguous).** btrfs returns 1 for a genuine fatal scrub error
  **and** for a deliberately cancelled scrub -- see below.

The gate's classification is asymmetric on purpose: a *busy* pool skips, but a
gate that cannot be read -- unreadable or unrecognized sysfs exclusive-op state,
or a `btrfs scrub status` the parser cannot classify -- is exit 1 and alerts.
Mapping probe breakage to "busy, retry later" would starve scrubs forever with
no operator signal. The entry freshness classifier is asymmetric the other way
for the same reason: nothing ambiguous may read as *fresh*, because an
unnecessary scrub is visible and self-limiting while silent starvation is not
([ADR 035](../../design/decisions/035-scrub-scheduling-by-freshness.md)).

## Why exit 1 cannot be read directly

`braid lock`, suspend, and shutdown stop `braid-scrub.service` mid-scrub; the
unit's `ExecStop` cancels the in-flight scrub via `btrfs scrub cancel`. When a
*real* scrub is cancelled, btrfs-progs exits **1**: in
`reference/btrfs-progs/cmds/scrub.c` `scrub_start`, an `ECANCELED` device
result does `++err`, and the function ends with `if (err) return 1`. So exit 1
is the *same* outcome as a genuine failure.

btrfs scrub **status** cannot break the tie either: `scrub_one_dev` sets
`canceled = !!ret` for *any* nonzero scrub ioctl, so a fatal scrub error
renders as `aborted` just like a deliberate cancel. The rendered status flag
cannot prove intent.

## The cancel-request marker (the discriminator)

The only authoritative signal for "this stop was deliberate" is braid's own
teardown intent, so the teardown records it:

1. The `ExecStop` script (`modules/braid/storage.nix#scrubCancelScript`)
   `touch`es `/var/lib/braid/scrub-cancel-requested` as its **first** action --
   before the `mountpoint -q` early-exit and before the cancel ioctl -- so the
   marker is present on every deliberate stop, including the mount-gone race.
2. `cmd_scrub_resume_or_start` (`cli/src/scrub_resume_or_start.rs`) **removes**
   any stale marker at entry, then on an ambiguous btrfs exit checks it:
   marker present -> `Cancelled` (service exits 0); marker absent -> the
   already-running refusal above -> a busy skip (exit 4); otherwise a genuine
   failure (service exits non-zero -> `onFailure`).

The marker is checked first, so a deliberate stop is never reported as someone
else's scrub. Both keep the unit off `onFailure`, so the difference is
invisible in the exit code but not in the journal, and only one of them sends
the operator hunting a hand-run scrub.

Ordering is race-free: the entry-remove runs when the scrub first starts (long
before any stop), and `ExecStop` writes the marker *before* issuing the cancel
that makes btrfs return 1, so the marker is present at the runner's post-exit
check **iff** a stop is in flight for this run.

The entry cleanup is **fail-closed**
([safety-heuristics](../../dev/safety-heuristics.md#mutation-safety-heuristics)):
it tolerates only `NotFound`. Any other removal error (the path is a directory,
`EACCES`, `EIO`, ...) errors out *before* btrfs runs, because a marker this run
could not clear would later turn a genuine exit 1 into `Cancelled` and swallow
the very failure the feature exists to alert on. Marker presence is tested with
`Path::exists()`, which coerces any I/O error to `false`, so the only route to
`Cancelled` is an unambiguously present marker -- absence or read ambiguity
falls through to the failure path.

## The scrub-failed flag (the alert source)

A genuine failure fails the unit, firing `onFailure = [ braid-scrub-failed.service ]`
(gated on `monitor.enable`). That oneshot mirrors the smartd hook:

1. `touch /var/lib/braid/scrub-failed` -- the durable flag.
2. `systemctl start braid-alert.service` -- the immediate beep.

The flag is a non-counter event source, so it is modeled exactly on
`smartd-alert`: `braid monitor` reads it each cycle and latches
`AlertCause::ScrubFailed` (Critical -> exit 1 -> beep), `braid status` surfaces
it from the flag immediately (before the next poll), and `braid ack` clears the
flag, the latch slot, and the beeper. `ScrubFailed` serializes as
`{"type":"scrub_failed"}` in `status --json`.

## Gated on the monitor

The entire pipeline -- the `onFailure` reference, `braid-scrub-failed.service`,
the latch, and `braid ack` -- exists only when `braid.monitor` is enabled.
`autoScrub` with the monitor disabled is legitimate (an operator running their
own monitoring) but silent, so braid emits a build-time **warning** for the
`autoScrub.enable && !monitor.enable` combination rather than failing
evaluation.
