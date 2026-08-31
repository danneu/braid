---
intent: Record why scheduled scrubs are decided from btrfs's own scrub record rather than a calendar, and what that deleted.
status: Active
---

# Decision: Scrub scheduling by freshness

## Context

`braid.autoScrub` fired on calendar dates: `OnCalendar = "monthly"` with
`Persistent = true`. The operator's actual intent was never "scrub on the first
of the month" -- it was "the pool gets re-scrubbed about a month after the last
scrub". Those differ in ways an operator notices:

- A scrub run by hand on the 30th was followed by the scheduled scrub on the 1st.
- A scrub deferred because braid was busy, and retried days later, was still
  re-scrubbed on the next calendar boundary.
- The systemd timer stamp file was a second "when did we last scrub" record,
  which could disagree with btrfs's own -- and did, every time a scrub ran
  outside the timer.

btrfs already keeps the fact everyone wanted. Any scrub, whoever started it,
writes `/var/lib/btrfs/scrub.status.<fsid>` from its userspace side, and the
record survives a reboot. It is the same timestamp `braid status` has always
shown as "Last scrub".

## Decision

**btrfs's scrub record is the single scheduling anchor.** The timer becomes a
cheap poll; `braid-scrub.service` reads `btrfs scrub status` at entry and exits
0 when the last scrub is fresh. Everything that duplicated that fact is deleted
rather than kept consistent.

### The anchor and the predicate

The anchor is the timestamp btrfs reports on its started/resumed line -- the
**latest start-or-resume**. After a resumed scrub btrfs prints only
`Scrub resumed:`, and `cli/src/parse/btrfs_scrub_status.rs#parse_scrub_started_or_resumed`
folds either line into one field. Operator-facing wording therefore says "last
scrub started/resumed", never "started".

Anchoring on the start is deliberate, not a parser limitation. btrfs records no
finish time; deriving `start + duration` is skewed on resumed scrubs. The latest
start-or-resume approximates completion from below, so it errs toward scrubbing
slightly early -- and it is the timestamp `braid status` already shows, so the
operator and the scheduler do the same arithmetic on the same number.

`cli/src/scrub_resume_or_start.rs#classify_freshness` classifies one probe into
exactly one outcome:

| btrfs reports | Outcome | Service exit |
| --- | --- | --- |
| `Finished`, anchor present, `0 <= now - anchor < window` | Fresh | 0 |
| `Running` | AlreadyRunning | 0 |
| `Never`, `Aborted`, `Interrupted`, missing anchor | Due | runs the scrub |
| `Finished` with a future-dated anchor (clock skew) | Due, named in the journal | runs the scrub |
| `Unknown` (unparseable) | hard error | 1, alerts |

**The fail direction is the inverse of the busy gate's.** Nothing ambiguous may
read as fresh: an unnecessary scrub is visible and self-limiting, while silent
starvation is neither. The busy gate fails the other way for the same reason
read backwards -- an unreadable gate must not wave a scrub through. `Unknown` is
neither: a scheduler that cannot read the pool's record must not decide from it
in either direction (`ScrubStatusUnreadable`, matching `braid idle`).

### Probe once, classify once

The decision rests on exactly one `btrfs scrub status` probe, run **before any
pool-lock acquisition** and classified exactly once. Fresh is the outcome of
almost every poll, so it must not contend for a lock a `braid add` may be
holding, and it must not cost more than the one probe.

The busy gate keeps its three "braid is already working on this pool"
conditions (pool lock, btrfs exclusive operation, pending-op journal) and loses
its fourth. "Someone else is already scrubbing" was never a reason to *defer* a
scrub -- it is a reason the pool owes none -- and keeping it in the gate meant
asking the same question twice with two possible answers.

**Invocation-time collision.** An external scrub can start after the entry probe
reports Due but before braid's own `btrfs scrub resume`/`start` runs; btrfs then
refuses with exit 1. That is discriminated **solely** by the failed invocation's
own output matching btrfs's already-running rejection, never by a post-failure
status re-probe -- which would be racy in both directions: an external scrub
that finished first would turn a real collision into a false failure, and one
that started after a genuine braid failure would suppress an alert braid must
raise. A collision behaves like an entry-observed `Running`: exit 0 with a
journal line. Every other exit 1 keeps its classification.

### Exit-code contract

Fresh, Running, and Collision are **exit 0** (the journal line distinguishes
each from a real run). A gate-busy skip stays **exit 4**, now purely
informational: `SuccessExitStatus=[3 4]` and `StartLimitIntervalSec=0` remain,
while `RestartForceExitStatus` and `RestartSec` are deleted -- the poll is the
retry. Exit 3 (corruption found, scrub completed) and exit 1 (failure ->
`onFailure` -> alert) are unchanged. No new exit codes.

The fresh path prints
`scrub not due: last scrub started/resumed <ts> (N days ago); next due in M days`.

### Option surface

`braid.autoScrub.intervalDays` (positive integer, default 30) replaces
`autoScrub.interval` and `autoScrub.retryInterval`, both trapped with
`lib.mkRemovedOptionModule`. Days granularity keeps bad values unrepresentable;
it reaches the CLI as `--fresh-for-secs <intervalDays * 86400>` computed in Nix
on the `ExecStart` line, so the scrub units stay config-file-free (ADR 018
thin-systemd-layer). Seconds granularity exists in the flag only so tests can
override the unit with small values. The flag is **required**: a bare
`braid scrub-resume-or-start` is a usage error, not an unwindowed scrub.
Forcing a scrub early is `btrfs scrub start`.

Eval-time warnings (not assertions) fire below 7 days (HDD wear, ADR 015) and
above 180 days (bit-rot window, ADR 005).

### The timer

`braid-scrub.timer` keeps its `braid-online.service` binding triad and becomes
`OnActiveSec=30s`, `OnCalendar=hourly`, `AccuracySec=1min`, with **no
`Persistent`** and never `WakeSystem`. `OnActiveSec` pokes shortly after
unlock/boot -- 0 would race the tail of `braid unlock`, which still holds the
pool lock. `Persistent` is gone because its stamp file is precisely the second
schedule record this design deletes; catch-up is `OnActiveSec` plus the
predicate.

### No wakeup at all

braid registers no autosuspend wakeup of any class, and `autoScrub.enable`
toggles none. A suspended NAS is never woken to scrub. Scrubs are opportunistic:
they run when the machine is awake and the record is stale. A running scrub
still blocks suspend via `braid idle` (ADR 016).

The resume-after-suspend premise: a realtime timer elapse that passed during
suspend fires promptly on resume ("if a timer elapsed while the system was
suspended, it will be triggered shortly after resume", `systemd.timer(5)`), so
any wake from a suspend longer than an hour gets a prompt poll. That, plus
`braid idle` blocking suspend during a running scrub, is the whole
opportunistic-scrubbing story. It is not VM-testable and `reference/` carries no
systemd source, hence the citation.

### Deleted outright

- The systemd timer stamp file (`Persistent`).
- `wakeups.BtrfsScrub` in `modules/braid/auto-suspend.nix`.
- `braid-scrub-resume-trigger.service` and the `scrub-needs-resume` subcommand.
  With the service self-gating, the `OnActiveSec` poke resumes an aborted scrub
  within ~30s of unlock, and "pool-online must not start unscheduled scrubs"
  holds because fresh -> exit 0.
- `/var/lib/braid/scrub-deferred` and its whole lifecycle. Under an hourly poll
  it carries no information btrfs's record does not, and it is the weaker
  source: it can claim debt on a pool a hand scrub has since made fresh.

## Consequences

- **A cancelled or aborted scrub is resumed by the next hourly poll**, rather
  than staying cancelled until the next pool-online or calendar firing. An
  aborted record is never fresh, however recent. The operator's "not now" lever
  is pausing the timer (`systemctl stop braid-scrub.timer`), which lasts until
  the next unlock or boot; it is documented in the guides.
- **A scrub run outside braid suppresses the next automatic one.** That is the
  headline promise, and it is also why a hand scrub can no longer turn a poll
  into a failure or an alert.
- **A mostly-suspended NAS scrubs late.** Staleness is bounded by usage, not by
  wall clock. Accepted; see the rejected computed-wake design below.
- **btrfs's aggregate record hides partial-device scrubs**: a device added after
  the last scrub still reads `finished`. Accepted risk; the per-device parser
  exists if this ever needs closing.
- **A lost or wiped `scrub.status.<fsid>` reads as `Never`**, costing one
  unnecessary scrub. Benign by construction.

## Test strategy

- A pure classifier matrix (`freshness_matrix`) over every state at the boundary
  ages: window-1s fresh, exactly-window due, negative age due, missing anchor
  due, recent `Aborted` not fresh, and a finished-*resumed* record anchoring on
  the resume time.
- Runner-level tests that Fresh takes no pool lock and issues exactly one
  `BtrfsScrubStatus`, that a peer holding the lock still yields Fresh, that
  entry-observed `Running` exits without touching the pool, and that the lost
  race -- entry probe Due, invocation refused with btrfs's already-running text
  -- raises no failure while other exit-1 shapes still do.
- Live-tool behavior locks (`docs/dev/testing.md#live-tool-behavior-locks`):
  `tests/repro/btrfs-scrub-record-anchors-schedule.py` pins that a hand-run
  scrub moves the anchor and that the record survives a reboot;
  `tests/repro/btrfs-scrub-start-rejected-during-scrub.py` pins the refusal
  wording the collision classification depends on.
- VM tests age staleness only via small `--fresh-for-secs` unit overrides --
  never guest-clock changes, never rewriting btrfs's status file.

## Alternatives considered

### Computed wake via a `scrub-next-due` command

Rejected as machinery. braid would compute the next due instant from the anchor
and register it as an autosuspend wakeup so a suspended NAS woke itself to
scrub. Verified against `reference/autosuspend`: a poll timer registered as a
`SystemdTimer` wakeup would break suspend outright (monotonic fallback plus
`min_sleep_time`), and Command-class wakeups fail open, so a broken computation
would silently stop suspending rather than stop scrubbing. Revisit this if
opportunistic scrubbing proves insufficient in practice.

### Keep the timer stamp and the freshness predicate

Rejected. Two records that answer "when did we last scrub" will disagree, and
the stamp is the weaker one -- it knows only about scrubs the timer started.
Keeping both means keeping them consistent forever for no gained information.

### Re-probe `btrfs scrub status` after a failed invocation

Rejected. It looks like the obvious way to tell a collision from a failure, and
it is racy in both directions (above). The refusal text is the only observation
made at the moment the fact was true.

### Derive the anchor from `start + duration`

Rejected. It reads like a better approximation of "when did the scrub finish",
but it is skewed exactly on resumed scrubs -- where `duration` covers the
resumed segment, not the whole scrub -- and it would no longer match the
timestamp `braid status` shows the operator.

### A duration grammar for the interval

Rejected. `intervalDays` as a positive integer makes `"1h"`, `"monthly"`, and
every other unrepresentable-in-this-model value a type error rather than a
surprise, and the two warning boundaries are readable as plain numbers.

## See

- [ADR 005: Sane defaults](005-sane-defaults.md)
- [ADR 015: HDD defaults](015-hdd-defaults.md)
- [ADR 016: Auto-suspend](016-auto-suspend.md)
- [ADR 018: Systemd lifecycle](018-systemd-lifecycle.md)
- [Scrub failure alerts](../../internals/tool-behavior/scrub-failure-alerts.md)
- `cli/src/scrub_resume_or_start.rs#classify_freshness`
- `cli/src/scrub_resume_or_start.rs#cmd_scrub_resume_or_start`
