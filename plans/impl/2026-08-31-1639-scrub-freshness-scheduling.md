# Freshness-based scrub scheduling

## Context

`braid.autoScrub` fires on calendar dates (`OnCalendar = "monthly"`,
`Persistent=true`). The user's actual intent is "the pool gets re-scrubbed about
a month after the *last* scrub" -- so a manual `btrfs scrub` should count and
push the next auto-scrub out. Today it doesn't: a hand scrub on the 30th is
followed by the calendar scrub on the 1st, a deferral retried days later is
still re-scrubbed on the next calendar boundary, and the timer stamp file is a
second "when did we last scrub" record that can disagree with btrfs's own.

The redesign: the timer becomes a cheap poll; `braid-scrub.service` reads
`btrfs scrub status` at entry and exits 0 when the last scrub is fresh. btrfs's
userspace record (`/var/lib/btrfs/scrub.status.<fsid>`, survives reboot, written
by any scrub regardless of who started it) becomes the *single* scheduling
anchor. Everything that duplicated that fact -- the systemd timer stamp, the
`scrub-deferred` flag, the retry/restart apparatus, the resume trigger -- is
deleted rather than kept consistent.

Decided with the user (2026-08-31):
- **No scrub wakeup at all.** The autosuspend `wakeups.BtrfsScrub` entry is
  removed; a suspended NAS is never woken to scrub. Scrubs run when the machine
  is awake and the record is stale. (Computed-wake via a `scrub-next-due`
  command was designed and rejected as machinery; revisit if opportunistic
  scrubbing proves insufficient in practice.)
- **Full consolidation** of the redundant mechanisms (below).
- **`autoScrub.intervalDays`** integer option; no duration grammar anywhere.

## Design

### The decider

`cli/src/scrub_resume_or_start.rs#cmd_scrub_resume_or_start` probes
`btrfs scrub status` **once, at entry, before the pool lock**, and classifies
the result exactly once into Fresh / AlreadyRunning / Due. The gate's fourth
condition (the running-scrub probe added in `56738f9e`) merges into this
classification; the gate keeps its three busy conditions (pool lock, btrfs
exclusive op, pending-op journal). Rationale: the fresh outcome is true on
almost every poll, must not contend for the pool lock, and one probe with one
classification removes the same-request-three-meanings hazard by construction.

Freshness predicate:
- **Fresh** iff state is `Finished`, the anchor is present, and
  `0 <= now - anchor < intervalDays`. The anchor is the timestamp btrfs's
  aggregate status reports on its started/resumed line -- the *latest
  start-or-resume*: after a resumed scrub btrfs prints only `Scrub resumed:`
  and the parser folds either line into one field
  (`cli/src/parse/btrfs_scrub_status.rs#parse_scrub_started_or_resumed`).
  Operator wording (journal line, ADR, guides) must say "last scrub
  started/resumed", not "started". This is deliberate, not a parser
  limitation: btrfs records no finish time, deriving `start + duration` is
  skewed on resumed scrubs, and the latest start-or-resume approximates
  completion from below -- it errs toward scrubbing slightly early (safe) and
  matches the "Last scrub" timestamp `braid status` already shows, so
  operators and scheduler do the same arithmetic.
- `Never`, `Aborted`, `Interrupted`, missing `started_at`, negative age (clock
  skew, with a journal line naming it) -> **Due**. Nothing ambiguous may read
  as fresh: the failure direction here inverts the busy gate's (an unnecessary
  scrub is visible and self-limiting; silent starvation is not).
- `Running` -> **exit 0** with a journal line: a scrub in flight means nothing
  is owed. (Distinct from gate-busy exit 4 = "owed but blocked".)
- **Invocation-time collision:** an external scrub can start after the entry
  probe reports Due but before braid's `btrfs scrub resume`/`start` runs;
  btrfs then refuses with its already-running rejection (exit 1). That is a
  distinct **Collision** outcome, discriminated *solely* by the failed
  invocation's own output matching btrfs's narrowly pinned already-running
  rejection -- never by a post-failure status re-probe, whose observation is
  racy in both directions (an external scrub finishing before the re-probe
  would turn the collision into a false failure; an external scrub starting
  after a genuine braid failure would suppress a real one). Collision behaves
  like Running at the boundary (exit 0, journal line); every other exit-1
  keeps its current classification. The pin: a fixture of the real rejection
  output, plus a live-tool assertion locking the rejection's shape
  (`docs/dev/testing.md` live-tool behavior locks).
- `Unknown` / unreadable status -> hard error (exit 1, alerts), the codebase-
  wide rule (`ScrubStatusUnreadable`, `scrub-needs-resume`, `idle`).

Clock: `now` is a naive-local timestamp injected from `main.rs` (the `_at`
convention, e.g. `cli/src/monitor.rs#cmd_monitor_at`), built the same way the
TUI builds its local now -- promote that projection into `cli/src/util.rs` so
there is exactly one. `ScrubTimestamp` is naive local (btrfs ctime); comparing
against UTC would be the bug class the TUI already documents. UTC-offset
fallback and DST are bounded at hours against a 30-day window: documented, not
handled.

### Exit-code contract

Fresh, Running, and Collision are **exit 0** (journal line distinguishes each
from a run).
Gate-busy skip stays exit 4, now purely informational -- `SuccessExitStatus=[3 4]`
and `StartLimitIntervalSec=0` remain; `RestartForceExitStatus` and `RestartSec`
are deleted (the poll is the retry). Exit 3 (corruption) and 1 (failure ->
`onFailure` -> alert) unchanged. No new exit codes.

Observability: the fresh path prints
`scrub not due: last scrub started/resumed <ts> (N days ago); next due in M days`
(ASCII). `braid status` is unchanged (it already shows the anchor); no next-due
display in this change.

### Option and flag surface

- `braid.autoScrub.intervalDays` (`lib.types.ints.positive`, default 30) in
  `modules/braid/options.nix`. Description must say: measured from the last
  scrub btrfs recorded; hand scrubs count; not a calendar schedule.
- Reaches the CLI as `--fresh-for-secs <intervalDays * 86400>` computed in Nix
  on the `ExecStart` line. Scrub units stay config-file-free (ADR 018
  thin-systemd-layer; `--mount` / `--deadline-secs` precedent). Seconds
  granularity in the flag exists so tests can override the unit with small
  values; days granularity in the option keeps bad values unrepresentable.
  The flag is **required** -- a bare `braid scrub-resume-or-start` is a usage
  error, not an unwindowed scrub; force-early stays `btrfs scrub start`.
- `lib.mkRemovedOptionModule` traps for `autoScrub.interval` and
  `autoScrub.retryInterval` with migration text; the `interval` trap must also
  name the capability loss for time-of-day expressions (off-peak is `Nice=19`
  + idle I/O now, not scheduling).
- Eval-time `warnings` for `intervalDays < 7` (HDD wear, ADR 015) and `> 180`
  (bit-rot window, ADR 005). Warnings, not assertions.

### Timer and units (`modules/braid/storage.nix`)

`braid-scrub.timer` keeps its `braid-online.service` binding triad and becomes:
`OnActiveSec=30s` (poke shortly after unlock/boot; 0 would race the tail of
`braid unlock` holding the pool lock), `OnCalendar=hourly`,
`AccuracySec=1min`, **no `Persistent`** (the stamp file is the second schedule
record this design deletes; catch-up is now `OnActiveSec` + the predicate),
never `WakeSystem`. A realtime hourly elapse that passed during suspend fires
promptly on resume, so any wake from a >1h suspend gets a prompt poll -- that,
plus `braid idle` blocking suspend during a running scrub, is the whole
opportunistic-scrub story.

Deleted outright:
- `wakeups.BtrfsScrub` in `modules/braid/auto-suspend.nix` (per the user's
  wake decision). Verified against `reference/autosuspend`: a poll timer as a
  `SystemdTimer` wakeup would break suspend (monotonic fallback +
  `min_sleep_time`), and Command-class wakeups fail open -- both recorded in
  the ADR as rejected alternatives.
- `braid-scrub-resume-trigger.service` and the `scrub-needs-resume` subcommand
  (`cli/src/scrub_needs_resume.rs`): with the service self-gating, the
  `OnActiveSec` poke resumes an aborted scrub within ~30s of unlock, and
  "pool-online must not start unscheduled scrubs" holds because fresh -> 0.
- `/var/lib/braid/scrub-deferred` and its whole lifecycle
  (`record_deferral`/`clear_deferral`/`scrub_deferral_pending`,
  `StatePaths::scrub_deferred`, both error variants): under an hourly poll it
  carries no information btrfs's record doesn't, and it is the weaker source
  (it can claim debt on a pool a hand scrub has since made fresh).
- `cli/src/lock.rs` teardown list drops the deleted trigger unit.

## Invariants

1. The scheduling decision rests on exactly one `btrfs scrub status` probe,
   run before any pool-lock acquisition and classified exactly once; no path
   -- Collision included -- re-probes status to decide or revise an outcome.
   The run path's post-spawn registration confirmation
   (`cli/src/scrub_resume_or_start.rs#confirm_scrub_registered`) answers a
   different question ("did my scrub register") and is out of scope.
2. Only `Finished` + a present anchor + age in `[0, window)` yields Fresh.
3. The Fresh and entry-observed Running paths mutate nothing: no pool lock, no
   btrfs command beyond the probe, no state-dir write. (Collision is exit 0
   but is not such a path: it has, by definition, issued the one refused
   resume/start; it still writes no state.)
4. `ScrubState::Unknown` / unreadable status is a hard error on every path.
5. No clock read below `main.rs`; no production timing constant test-gated.
6. The freshness window reaches the CLI only via the flag; no scrub unit reads
   a config file.
7. braid registers no autosuspend wakeup of any class when this lands;
   `autoScrub.enable` toggles no wakeup.
8. A scrub started outside braid both suppresses the next auto-scrub (fresh
   record) and never turns a poll into a failure or alert.

## Non-goals / accepted risks

- **Non-goal:** waking a suspended NAS for a due scrub. Accepted risk: a
  mostly-suspended NAS scrubs late -- staleness is bounded by usage, not wall
  clock. Revisit = the rejected computed-wake design in the ADR.
- **Non-goal:** next-due display in `braid status`.
- **Accepted risk:** btrfs's aggregate record hides partial-device scrubs
  (a device added after the last scrub still reads `finished`); the per-device
  parser exists if this ever needs closing.
- **Accepted risk:** lost/wiped `scrub.status.<fsid>` reads as `Never` -> one
  unnecessary scrub. Benign by construction.

## Docs and ADRs

- **New ADR 035 "scrub scheduling by freshness"** (`Active`): the anchor and
  predicate, probe-once, the inverted fail direction, exit-0-for-fresh, the
  deletions and why each redundant record had to go, the no-wakeup decision
  with the verified autosuspend facts and the rejected computed-wake design.
  Two consequences it must record: a cancelled/aborted scrub is now resumed by
  the next hourly poll rather than staying cancelled until the next
  pool-online or calendar firing (the operator's "not now" lever is pausing
  the timer, documented in the guides); and the resume-after-suspend premise
  -- a realtime timer elapse that passed during suspend fires promptly on
  resume -- pinned with a citation to the systemd.timer man page, since it is
  not VM-testable and `reference/` carries no systemd source.
- Amend (all `Active`): ADR 018 (timer/gate/deferral/trigger sections, exit
  table), ADR 005 (interval default row: cadence unchanged, anchor changed),
  ADR 015 ("monthly" -> "30 days since the last scrub"), ADR 016 ("only
  scheduled wakeup is the scrub timer" -> braid schedules no wakeups; scrubs
  are opportunistic; a running scrub still blocks suspend).
- Guides sweep: `nixos-configuration.md` (option table, example, migration
  note), `day-to-day-nas-usage.md` (calendar-retiming advice is now false;
  hand scrubs count; force-early = `btrfs scrub start`; stop-for-now =
  `btrfs scrub cancel` + `systemctl stop braid-scrub.timer` until re-unlock or
  next boot, else the next poll resumes it), `power-management.md`
  (no scrub wakes), `troubleshooting.md` ("my scrub didn't run" -> journal
  `scrub not due` line), `monitoring-and-alerts.md`,
  `internals/tool-behavior/scrub-failure-alerts.md` (gate = three conditions +
  entry classifier; exit table; deferral paragraph gone),
  `design/principles.md` scrub-scheduling phrasing, `docs/commands/status.md`
  (status shows the anchor, not next-due), `README.md`, `SUMMARY.md` (ADR 035).

## Verification

- **Pure classifier matrix** (table test, no mocks): every state x boundary
  ages (window-1s fresh, exactly-window due, negative age due), missing
  timestamp, recent `Aborted` not fresh, future-dated record runs, and a
  finished-*resumed* record (only `Scrub resumed:` present) anchoring on the
  resume time.
- **Runner-level** (`Rig`/`MockRunner`): Fresh takes no pool lock and issues
  exactly one `BtrfsScrubStatus`; Due reaches the unchanged run path; Running
  exits 0; the lost race -- entry probe reports Due, the resume/start
  invocation returns btrfs's already-running rejection -- exits 0 and raises
  no failure, while other exit-1 shapes still fail. Existing tests re-based on
  an injected `now` + a dated finished fixture (`scrub_status_finished_at`),
  not by mutating the shared fixture.
- **Live-tool behavior lock** (required: the design newly depends on tool
  *semantics*): VM assertions that a hand-run `btrfs scrub start -B` moves the
  `Scrub started:` line, that the record survives a reboot, and that btrfs's
  refusal to start/resume over a running scrub still has the shape the
  collision classification pins.
- **VM tests** (staleness aged only via small `--fresh-for-secs` unit
  overrides -- never guest-clock changes, never rewriting btrfs's status file):
  - `auto-scrub`: option->flag mapping (`--fresh-for-secs` default and custom),
    new timer directives incl. negative asserts (no `Persistent`, no
    `OnCalendar=monthly`, no `WakeSystem`), dead-name guards for the deleted
    trigger unit, absence of `RestartForceExitStatus`/`RestartSec`.
  - `scrub-lifecycle`: re-mechanized off the timer stamp -- fresh record
    suppresses scrub on unlock (flagship), stale record scrubs on unlock,
    never-scrubbed pool scrubs via the `OnActiveSec` poke, aborted scrub
    resumes via the poke, concurrency: a poke during a running scrub starts no
    second scrub and raises no alert.
  - `scrub-skip-retry`: busy skip exits 4 with no alert and the *next poll*
    retries (timer unmask mechanism replaces the retryInterval arc); fresh
    poll is a no-op touching nothing.
  - `scrub-alert`: unchanged rows; preamble notes exit 0 now includes fresh.
  - `braid-auto-suspend`: no `[wakeup.BtrfsScrub]` section exists.
  - `eval-*`: removed-option traps fire with the migration text; warning
    boundaries pinned on both sides: `intervalDays = 6` warns, `7` does not,
    `180` does not, `181` warns.
- Gates: `just test-rust`, `just test-vm braid-auto-scrub scrub-lifecycle
  scrub-skip-retry scrub-alert braid-auto-suspend <new eval checks>`,
  `just test-parsers` (leans on existing `ScrubState::Finished` fields), docs
  checks (`just check-docs`, `check-line-cites`, `check-docs-see-paths`,
  `check-output-ascii`), clippy + fmt.

## Critical files

`cli/src/scrub_resume_or_start.rs`, `cli/src/main.rs`, `cli/src/util.rs`,
`cli/src/state_paths.rs`, `cli/src/scrub_needs_resume.rs` (deleted),
`cli/src/lock.rs`, `modules/braid/options.nix`, `modules/braid/storage.nix`,
`modules/braid/auto-suspend.nix`, `modules/braid/default.nix`,
`tests/module/{auto-scrub,scrub-lifecycle,scrub-skip-retry,scrub-alert}.{nix,py}`,
`tests/module/braid-auto-suspend.py`, `tests/eval/`, `flake.nix`,
`docs/design/decisions/{005,015,016,018}-*.md` + new `035`, guides per above.

## Commit progress

- [x] 1. fix(scrub): treat invocation collisions as already running
- [ ] 2. fix(auto-suspend): stop waking for scrub timers
- [ ] 3. feat(scrub): schedule scrubs by recorded freshness

## Implementation notes

### Commit 1 (invocation collisions)

- **Collision exits 4, not 0, in this slice.** The design's exit-0 boundary for
  Collision belongs to the same change that moves entry-observed `Running` to
  exit 0 (commit 3). Until then the gate's already-running condition is a
  `Skipped` (exit 4, deferral recorded, no alert), so the collision -- the same
  fact observed a moment later -- returns the *same* `Skipped` with the same
  shared reason constant rather than a new variant that would mean nothing
  different yet. Commit 3 promotes both together, from one place. This is what
  the entry's title ("as already running") prescribes.
- **The rejection needs btrfs's userspace record, not just a kernel scrub.**
  Upstream gates the refusal on `is_scrub_running_in_kernel(...) &&
  is_scrub_running_on_fs(...)`; the second half reads
  `/var/lib/btrfs/scrub.status.<fsid>`. The starting process writes the
  all-zero record *before* it forks, so the record is in place as soon as the
  external `btrfs scrub start` returns -- the collision window is covered in
  practice. But `btrfs scrub status` still prints `no stats available` until
  the child stamps `t_start`, so the printed status is not a usable
  precondition probe; the live-tool test asserts on the refusals themselves.
- **The live-tool lock needs LUKS as a throttle.** Unencrypted on
  linux-builder, a 3 GiB payload scrubs in ~1.5 seconds -- no window to land
  the refusals in. `btrfs scrub start --limit 5m` was tried first and did not
  slow the kernel scrub at all. The test therefore mirrors
  `tests/repro/btrfs-replace-rejected-during-scrub.py` exactly (LUKS + 3000 MiB
  on 4096 MiB disks, ~7-15 second window). Recorded because "LUKS is scenery
  for a btrfs-progs wording lock" is the obvious simplification and it is
  wrong.

## Follow Up

- `btrfs scrub start --limit <rate>` does not throttle the kernel scrub on the
  pinned btrfs-progs/kernel pair (observed: `--limit 5m` on a ~1 GiB RAID1
  scrub still finished in under 1.5s, both devids). If braid ever wants a
  bounded-rate scrub -- or a cheaper live-tool window than the LUKS throttle --
  this needs its own investigation against
  `reference/btrfs-progs/cmds/scrub.c#write_scrub_device_limit` and the
  per-device `scrub_speed_max` sysfs knob.
- A collision whose external scrub started so recently that btrfs has not yet
  written its status record slips past `is_scrub_running_on_fs`; the kernel
  then refuses braid's scrub with different wording, which stays a genuine
  failure and alerts. The window is sub-second and the plan's design already
  accepts "every other exit-1 keeps its current classification", but it is the
  one shape the collision classifier does not cover.
