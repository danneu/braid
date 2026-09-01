# Bounded-rate scrub throttle for VM tests

## Problem

Tests that need a scrub to stay running for a window of seconds currently get
that window by accident of hardware: the two live-tool locks
`tests/repro/btrfs-scrub-start-rejected-during-scrub.py` and
`tests/repro/btrfs-replace-rejected-during-scrub.py` use LUKS overhead plus a
3000 MiB payload on 4096 MiB disks to stretch an unthrottled scrub to a
~7-15 second window at the builder's observed ~400 MiB/s. The window is a
heuristic (rate x payload both unmeasured per run), the disks and payload are
an order of magnitude larger than the assertions need, and the tests' preambles
must argue "LUKS is not scenery -- it is the throttle" to stop the obvious
simplification.

The kernel has a purpose-built knob for exactly this:
`/sys/fs/btrfs/<fsid>/devinfo/<devid>/scrub_speed_max`, read live per-bio
(0 = unlimited). Verified on the pinned pair (btrfs-progs 6.19.1 /
kernel 6.18.33) -- do not re-investigate:

- `scrub start` owns the knob for the duration of the run. `scrub_start`
  saves each device's old limit, writes its `--limit` value (0 when the flag
  is absent) to *every* device, spawns the scrub threads, and only then
  restores the saved values -- one device at a time, each immediately before
  that device's `pthread_join`. Device 0 is restored within milliseconds
  (which is why `--limit` reads as a silent no-op while progs still prints
  `(limit ...)`), but device N is restored only after devices 0..N-1 have
  finished scrubbing. So a plain `scrub start` runs every device but the first
  at rate 0 for the whole run, discarding any limit preconfigured via
  `scrub limit`. Same ordering on upstream master. Source:
  `reference/btrfs-progs/cmds/scrub.c#scrub_start`,
  `reference/linux/fs/btrfs/scrub.c#scrub_throttle_dev_io`.
- The refusals these tests lock are per-device, not fs-wide: `btrfs_scrub_dev`
  returns `-EINPROGRESS` on `dev->scrub_ctx` for the named device, which the
  replace ioctl surfaces as `SCRUB_INPROGRESS`
  (`reference/linux/fs/btrfs/scrub.c#btrfs_scrub_dev`,
  `reference/linux/fs/btrfs/dev-replace.c#btrfs_dev_replace_start`). An
  aggregate wall-time floor is therefore not evidence that any particular
  device is still scrubbing. (progs' `Scrub is already running.` check sits
  ahead of the limit writes in `scrub_start`, so a refused invocation does not
  disturb a running scrub's limits.)
- Writing the sysfs knob directly throttles exactly: 20 MiB/s per device on a
  2-disk RAID1 gave 40.06 MiB/s aggregate, 19.99s for a 400 MiB payload.
- `btrfs scrub limit -a -l <rate> <mnt>` sets the same knob, and the value
  survives subsequent `scrub start` invocations (the revert restores it, not
  clears it) -- but, per the bullet above, it is not in force *during* a plain
  run. It is in-memory per-mount state: reset on unmount/reboot.
- The knob exists since kernel 5.14; initial value 0.

## Decision

The ideal is also the cheap one here: throttle with the kernel's own
per-device rate knob and drop the incidental throttles. Concretely:

1. **A shared test helper that owns both the rate and the launch** (same
   pattern as `tests/module/dm_delay_helpers.py`: a python file concatenated
   into `testScript` by the `.nix`). Callers never run `btrfs scrub start`
   themselves. The helper:
   - persists the rate with `btrfs scrub limit -a -l <rate> <mnt>` -- the
     operator-legible surface;
   - asserts the sysfs knob readback on every device, so the test dies at
     setup if either the subcommand or the kernel knob vanishes on a future
     pin, rather than going slow-path green or vacuous;
   - launches the background scrub with `btrfs scrub start --limit <the same
     rate>`.

   Passing the same nonzero rate in both places is what makes the window
   sound: `scrub_start`'s temporary write and its sequential restore then both
   write that rate, so no device is ever set to 0 and every device is bounded
   from scrub-thread launch onward. Configuring the rate separately and
   launching with a plain `scrub start` would leave every device but the first
   unlimited for the entire run (see Problem); owning the launch is what stops
   a caller from reintroducing that.

2. **Convert both rejected-during-scrub repro locks to unencrypted RAID1 +
   throttle.** With a real throttle, LUKS in those tests reverts to scenery:
   the wording each test locks (`Scrub is already running.` /
   `scrub is in progress`) is produced by btrfs-progs' status-file check and
   the kernel replace ioctl respectively -- neither depends on the block stack
   beneath btrfs. The preambles' "LUKS is the throttle, not scenery" argument
   was true and is exactly what this plan deletes; the preambles (.py and
   .nix) are rewritten to record the knob as the throttle and to cite the new
   behavior lock (item 3) as the ground the throttle stands on. Disks and
   payload shrink to what the assertions need; the window becomes
   deterministic (payload / rate) instead of builder-speed folklore.

3. **Harden the investigation repro into a registered live-tool behavior
   lock** (per `docs/dev/testing.md#live-tool-behavior-locks`): the untracked
   `tests/repro/btrfs-scrub-limit-noop.{nix,py}` become a committed,
   `flake.nix`-registered repro with the standard Intent / Why it exists /
   Scenario preamble, locking the three tool properties every throttled test
   now rests on:
   - the sysfs knob exists and actually bounds scrub rate: a wall-time floor
     on a known payload, launched the way the helper launches;
   - the helper's launch shape is both necessary and sufficient, observed
     per-device rather than in aggregate: with a rate preconfigured via
     `scrub limit`, a plain `scrub start` leaves the last device's knob
     reading 0 while the first reads the configured rate, whereas a
     `--limit <same rate>` launch leaves every device's knob at that rate for
     the whole run;
   - the configured limit is intact after the run (revert-restores, not
     clears).

   The middle subtest is also the canary: a future progs pin that fixes the
   restore ordering flips its first half and fails loudly, prompting a
   re-evaluation -- possibly simplifying the helper -- instead of braid
   silently carrying a stale workaround claim.
   The investigation phases (baseline timing printouts, loose asserts) are
   deleted, not committed.

4. **Module tests stay on dm-delay.** `scrub-lifecycle`, `scrub-alert`, and
   siblings already throttle with dm-delay under the pool. The knob cannot
   replace it there: its per-mount lifetime dies on unmount, and those tests
   lock/unlock (which unmounts) mid-window -- dm-delay survives that, the knob
   silently would not. Their recent timing race (a sampled `Status: running`
   racing a completing scrub) was already fixed structurally by asserting on
   btrfs's anchor rather than by needing a longer window, so the knob buys
   them nothing.

## Invariants

- I1: A test that needs a live-scrub window gets it from a rate that is in
  force on every participating device from scrub-thread launch onward
  (window = payload / configured rate), not from incidental slowness of the
  block stack or builder, and not from an aggregate wall time that only some
  devices contribute to.
- I2: No throttled test can pass vacuously if the throttle stops working:
  the helper asserts the knob readback at setup, and each test's assertions
  are themselves the window precondition (the existing "the refusals are the
  precondition check" property is preserved -- a finished scrub turns the
  expected refusals into successes and the test fails loudly). A test that
  gets its live-scrub window from this knob starts its scrub only through the
  helper, so an unthrottled launch is not something such a caller can spell.
  (The behavior lock of Decision item 3 launches plainly on purpose -- that is
  the property it locks, not a window it depends on.)
- I3: The tool properties the throttle rests on -- the knob bounds scrub rate;
  a `--limit <same rate>` launch keeps every device at that rate for the whole
  run while a plain launch does not; the value is restored, not cleared -- are
  locked by a registered live-tool repro. That repro is a pin-bump gate, not
  automatic CI: `.#checks` excludes `repro-*` (`flake.nix`) and no workflow
  runs `just test-repro`, so the btrfs-progs / kernel pin-bump procedure in
  [parser-compatibility.md](docs/dev/parser-compatibility.md) names it as a
  required manual step.
- I4: The knob is per-mount in-memory state; no test may rely on a limit
  across an unmount/remount (or braid lock/unlock) boundary. Tests that need
  a throttle across that boundary use dm-delay.
- I5: Test preambles state the actual throttle mechanism; the stale
  "LUKS is the throttle" prose survives nowhere except as history in
  `plans/impl/`.

## Proof obligations

- PO1 (I3): the new behavior-lock repro, registered in `flake.nix` and green
  via `just test-repro`; and `docs/dev/parser-compatibility.md`'s pin-bump
  procedure names running it as a required step.
- PO2 (I1, I2): both converted repro locks green via `just test-repro`, each
  retaining its refusal-as-precondition failure mode.
- PO3 (I2): the helper's readback assert demonstrably fires on a bogus knob
  path (verify once during implementation; no committed test needed --
  the behavior-lock repro covers the real-knob half permanently).

## Non-goals / Accepted risks / Rejected ideas

- Non-goal: a user-facing braid "gentle scrub" / bounded-rate scrub feature.
  Future work at most; the follow-up bullet in
  `plans/impl/2026-08-31-1639-scrub-freshness-scheduling.md` is resolved on
  the test side by this plan and stays as the pointer for the feature side.
- Non-goal: converting the dm-delay module tests (see Decision item 4).
- Non-goal: editing the shipped impl plan's Follow Up section -- impl plans
  are records.
- Accepted risk AR1: the restore-ordering canary fails on the pin that fixes
  progs. That is the point -- it converts an upstream fix from silent drift
  into a visible decision -- but it is a test that breaks on an improvement;
  the preamble must say exactly what to do when it fires. The throttled tests
  themselves are unaffected by such a fix: a `--limit <same rate>` launch
  stays correct under either ordering.
- Accepted risk AR2: repro checks do not run in CI, so a pin bump can land
  without executing this behavior lock. Out of scope to change here; I3's
  documented pin-bump step is the mitigation.
- Rejected: keeping LUKS in the rejected-during-scrub locks as "realistic
  braid stack". The locks pin btrfs-progs wording and a kernel ioctl result,
  neither of which reads the device stack; realism there is scenery cost, and
  the braid-stack-under-LUKS path has its own module-test coverage.
- Rejected: direct sysfs write as the helper's setting surface. It works, but
  the subcommand is more legible in a test and locks an additional
  user-visible surface; the sysfs readback assert keeps the fail-loud
  property either way.

## Follow-up

- File an upstream bug against kdave/btrfs-progs for the
  `scrub start --limit` revert-before-join no-op, citing
  `cmds/scrub.c#scrub_start` ordering.
- Optional one-line pointer in `docs/dev/testing.md` (patterns section) so
  future test authors find the throttle helper instead of reinventing a
  payload-size throttle.

## Implementation discretion

- Exact rates, payload sizes, disk sizes, timeouts, and the helper's file
  name/location -- constrained only by I1/I2 and by keeping the converted
  tests' windows comfortably larger than their assertion phases.
- The hardened behavior lock's name and subtest structure, provided it
  discharges all three I3 properties.

## Commit progress

- [x] 1. test: lock bounded-rate scrub launch behavior
- [x] 2. test: use bounded-rate windows in scrub refusal repros

## Implementation notes

- The hardened lock is `tests/repro/btrfs-scrub-limit-bounds-rate.{nix,py}`
  (registered as `repro-btrfs-scrub-limit-bounds-rate`), renamed from the
  investigation's `btrfs-scrub-limit-noop`: the committed test locks the
  bounded launch shape, not just the `--limit` no-op.
- Framework gotcha found while making the lock green: the non-`-B`
  `btrfs scrub start` forks and the child inherits stdout, so an unredirected
  `machine.succeed` blocks until the whole scrub finishes -- every "mid-run"
  sample silently becomes a post-run one (this produced a false canary
  failure before diagnosis; the tool properties themselves reproduced exactly
  as the plan states). Both launches redirect to `/dev/null`. Entry 2's
  helper must launch with the same redirect or callers' live-scrub windows
  quietly vanish.
- Sizes chosen under Implementation discretion: 2x1024 MiB disks, 400 MiB
  payload at `20m` per device -- a ~20s deterministic window with a 10s
  wall-time floor.
- The helper is `tests/repro/scrub_throttle_helpers.py`, concatenated into
  `testScript` per the `dm_delay_helpers.py` pattern. It lives in
  `tests/repro/` because both callers are repros; the behavior lock does not
  use it (it launches by hand on purpose -- that launch shape is the property
  it locks). The converted repros reuse the lock's sizing: 1024 MiB disks,
  400 MiB payload at 20 MiB/s per device (~20s window).
- With LUKS gone, `btrfs fi show` prints kernel device paths instead of
  `/dev/mapper/*`, so the replace repro parses disk2's devid by matching the
  `readlink -f` target of the by-id symlink.
- PO3 verified and not committed: pointing the helper's readback at
  `scrub_speed_max_bogus` failed the converted scrub-start repro at helper
  setup (`must succeed: cat .../scrub_speed_max_bogus` exit 1), before any
  scrub launched.
