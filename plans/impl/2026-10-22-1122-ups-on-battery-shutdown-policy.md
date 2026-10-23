# UPS on-battery shutdown policy

## Problem

braid shuts the host down on battery only when the UPS firmware raises low
battery (LB). That is the latest possible moment: often about 120 s of
estimated runtime left on an aging battery, after a deep discharge.

The documented way to shut down earlier does nothing. Setting
`battery.runtime.low` or `battery.charge.low` in `ups.conf` (`override.*`)
only publishes the value. NUT adds LB from those values only when the driver
has the `ignorelb` flag (`reference/nut/drivers/dstate.c#status_commit`). The
guide (`docs/guides/ups.md#what-happens-on-low-battery`), ADR 020 (resolved
question 3), and the test fixture comment in `tests/module/lib/ups-fixture.nix`
all point users at it. caja set both overrides; its 180 s / 30 % budget was
never enforced.

## Desired outcome

A home NAS rides out short outages and shuts down early in long ones, with
the battery still about half full:

```nix
braid.ups = {
  enable = true;
  shutdownAfter = 300;      # seconds on battery
  shutdownBelowCharge = 50; # percent
};
```

Shutdown fires at whichever comes first. Firmware LB stays as the last-resort
backstop.

## Evidence for load-bearing premises

- `upsmon -c fsd` makes the running upsmon set FSD on the UPS and then run
  its normal shutdown: the root parent writes `POWERDOWNFLAG` and runs
  `SHUTDOWNCMD` (`reference/nut/clients/upsmon.c#forceshutdown`,
  `#doshutdown`, `#set_pdflag`). nixpkgs sets `POWERDOWNFLAG=/run/killpower`
  by default and ships `ups-killpower.service`, which runs
  `upsdrvctl shutdown` late in shutdown. `usbhid-ups` tries
  `shutdown.return` first (`reference/nut/drivers/usbhid-ups.c#upsdrv_shutdown`).
- upsmon writes its PID file as root at the compiled default path, before
  dropping to the upsmon user, and `sendsignal` reads that same path
  (`reference/nut/common/common.c#writepid`, `#sendsignal`). The PID is the
  unprivileged child's. Signalling it and passing NUT's process-name check
  (`/proc/<pid>/exe`) need the same uid, not root. So a guard running as the
  upsmon user needs no capabilities, and ADR 034's cleared child environment
  does not change the PID path.
- `upsc` reads are anonymous; the guard needs no upsd credential.
- `ignorelb` makes the driver refuse to start without charge/runtime data
  (`reference/nut/drivers/main.c`), and drops the device's own LB.

## Decision

- D1. A braid-owned guard service polls the UPS and forces shutdown through
  upsmon (`upsmon -c fsd`) when the UPS has been on battery for
  `shutdownAfter`, or when `battery.charge` is below `shutdownBelowCharge`
  while on battery. One mechanism serves both triggers. braid does not take
  over `NOTIFYCMD`, `NOTIFYFLAG`, or `schedulerRules`, so ADR 020's decision
  to defer alerts is unaffected.
- D2. The guard's decision is a pure function of (UPS reading, clock state,
  now, policy), owned by the CLI and unit-tested over the case matrix. The
  service loop only polls, applies it, persists the clock, and triggers.
- D3. The guard reads its policy from `/etc/braid/config.json` (the `ups`
  block that already carries `name`), like every other braid consumer.
- D4. The guard runs as the upsmon user (`power.ups.upsmon.user`) with an
  empty capability set, under ADR 033's base profile, with loopback network
  access for upsd. It is not pool-bound: it runs whenever UPS support is
  enabled, pool online or not.
- D5. Options are `braid.ups.shutdownAfter` (positive integer seconds, or
  null) and `braid.ups.shutdownBelowCharge` (integer percent, 1-99, or
  null). Integer seconds because the guard counts seconds; braid has nothing
  to parse. Null turns that trigger off; both null means the guard is not
  installed.
- D6. Defaults are on: `shutdownAfter = 300`, `shutdownBelowCharge = 50`
  (user decision). Either trigger can only shut down earlier than LB, so a
  wrong default errs toward safety. This changes behavior on upgrade for
  every existing UPS user. A user who wants a longer ride-through raises the
  value or sets it to null. The release notes call out the change.
- D7. `braid doctor` reports, when UPS support is enabled and a trigger is
  configured: the guard not running (same class as the existing
  braid-online-inactive check), and a charge floor that the UPS cannot
  enforce because it does not report `battery.charge`.
- D8. Docs: the guide's "what happens on low battery" becomes the shutdown
  policy, with the sizing rule (`shutdownAfter <= runtime-at-load x 0.5 -
  teardown - margin`, and a bigger UPS when that falls under about 2 min).
  Amend ADR 020 in place: the shutdown section names both triggers and the
  LB backstop, resolved question 3 is rewritten, and the advice about
  `battery.runtime.low` overrides is removed. Sync `README.md`, ADR 005's
  defaults table, and the fixture comment.

- D9. Because the defaults are on, every VM test that imports the UPS
  fixture gets a live guard. A test that sits on battery without testing
  shutdown (today `tests/module/ups-preflight-on-battery.nix`, which boots
  `OB`) sets both options to null, so a slow run can't power the VM off
  partway through.
- D10. The guard keeps its clock as a `CLOCK_BOOTTIME` timestamp in a file
  under `/run`. `CLOCK_BOOTTIME` counts time spent suspended and restarts at
  boot. `/run` is cleared at boot. Together they give I3 with no suspend hook
  and no boot-ID check. The wall clock and `CLOCK_MONOTONIC` are wrong here:
  NTP steps move the wall clock, and `CLOCK_MONOTONIC` stops during suspend,
  so a host that auto-suspends on battery (ADR 016) would get a fresh timer
  after resume.

## Invariants

- I1. braid forces shutdown only through upsmon's FSD path, never by calling
  `systemctl poweroff` itself. So `POWERDOWNFLAG`/killpower, the FSD flag in
  `ups.status`, and the existing SHUTDOWNCMD unwinding (ADR 018) apply the
  same way as on LB.
- I2. The on-battery clock starts on the first reading whose status contains
  `OB`. Only an affirmative `online` verdict from the shared
  `UpsSeverity` classifier stops it. A failed query, empty status, or
  unknown status never stops a running clock and never starts one. Same "OL
  is the proof" rule as mutation preflight.
- I3. Elapsed on-battery time survives a guard restart (rebuild switch,
  crash) within one boot, and counts time spent suspended. It does not
  survive a reboot.
- I4. The charge floor fires only on a readable `battery.charge` below the
  floor while the clock runs. An unreadable charge never counts as above
  the floor in any report, never blocks the timer, and is logged as a
  warning each time it changes state.
- I5. Firmware LB behavior is unchanged: no `ignorelb`, no `override.*`, no
  change to upsmon's critical-state handling.
- I6. Each trigger writes one ASCII journal line naming the reason (timer
  or charge floor) and the values that tripped it, before invoking upsmon.
- I7. A failed `upsmon -c fsd` is a loud error and is retried on the next
  poll. It is never treated as done.
- I8. Detection lag is bounded: the guard polls at most every 10 s, so a
  trigger fires within about 10 s of its condition holding. A UPS query that
  hangs does not stretch that bound for a timer that is already running.

## Proof obligations

- PO1 (I2, I4, D2). Rust unit tests over the decision function cover: clock
  start on OB, stop only on online, failed/unknown/COMMBAD readings that
  neither stop nor start the clock, timer expiry, charge floor with readable,
  missing, and unparseable charge, and both triggers at once.
- PO2 (I1, D1, D4). VM test with the production unit's sandbox, not a test
  override, and the fixture's dummy-ups: go OB, return to OL before a short
  timer expires, host stays up. Go OB again past the timer: the host shuts
  down cleanly, `braid-online` ExecStop completes in the previous boot's
  journal, and the trigger line (I6) is there.
- PO3 (I3, D10). In the same VM test, restart the guard mid-outage. The
  shutdown happens on the original deadline, not one timer length after the
  restart. After the reboot that follows the forced shutdown, the guard
  starts with no clock. A Rust test over the production clock-state code
  (not the decision function) shows the stored timestamp is read from
  `CLOCK_BOOTTIME` and round-trips through the state file.
- PO7 (I7, I8). Rust tests of the guard loop, with stub `upsc` and `upsmon`
  processes and injected time: a failing `upsmon -c fsd` is logged as an
  error and run again on the next poll; a `upsc` that hangs while a timer
  is running does not stop the trigger from firing within I8's bound.
- PO4 (I1, killpower). The forced-shutdown path in PO2 and the existing LB
  path in `ups-lb-clean-shutdown` both show `ups-killpower` ran in the
  previous boot's journal. If a VM cannot show it, say so in the test and
  leave it to FU2.
- PO5 (D7). Doctor unit tests: guard inactive with a trigger configured;
  floor set and `battery.charge` absent; both pass when healthy and are
  skipped when UPS is off.
- PO6 (D5). Module eval tests: both null installs no guard; out-of-range
  values fail evaluation; config.json carries the policy.

## Non-goals

- OOS1. Shutting down a host that is suspended on battery. Nothing runs
  while suspended; that gap exists today.
- OOS2. Alerting on OB or on a pending shutdown. Still deferred by ADR 020.
- OOS3. Networked NUT, secondaries, and multiple UPSes (ADR 020 scope).
- OOS4. Changing caja's config. It lives outside this repo (FU3).

## Accepted risks

- AR1. A timed shutdown can land during a pool mutation. Preflight already
  refuses mutations unless the UPS shows `OL`, so the mutation started on AC.
  This is the mid-mutation LB case that the `ups-lb-during-*` recovery tests
  already prove. Deferring the shutdown would only spend battery.
- AR2. If mains returns during a timed shutdown, and the UPS firmware ignores
  `shutdown.return` once on line power, the host stays off until someone
  powers it on. The timer makes this race more common than with LB. The
  guide documents it.
- AR3. If the guard dies or upsmon is down, only firmware LB remains, as
  today. Doctor (D7) reports the dead guard.
- AR4. No test suspends and resumes a host on battery. The suspended-time
  part of I3 rests on the kernel's definition of `CLOCK_BOOTTIME` (D10) and
  PO3's check that the guard reads it. A VM suspend/resume test would be
  slow and flaky for a property the kernel already guarantees.

## Rejected ideas

- RI1. `ignorelb` with `battery.*.low`. It drops the device's own LB and
  ShutdownImminent signals. The driver won't start if the UPS stops
  reporting charge or runtime.
- RI2. upssched timers through `NOTIFYCMD`. upssched sees only events, not
  charge levels, so the floor would need a second, polling mechanism. braid
  would also have to own `NOTIFYFLAG` and `schedulerRules`, which would clash
  with users' own alert setups.
- RI3. Writing firmware thresholds with `upsrw` (writable `battery.*.low`).
  It depends on the device, persists in the UPS outside the NixOS config,
  and cannot be tested in a VM.
- RI4. A guard that runs `systemctl poweroff` directly. It skips
  `POWERDOWNFLAG`, so the UPS never cycles power and the host would not
  come back when mains returns (I1).

## Implementation discretion

- The internal command name for the guard, and how the loop keeps a hung
  query from holding up a running timer (a query timeout or a separate
  deadline check both satisfy I8).

## Follow Up

- [ ] FU1. Show the on-battery clock and the shutdown deadline in
  `braid ups status` and the TUI. That needs the guard's state as a
  cross-process contract.
- [ ] FU2. Hardware canary under `tests/hw/`: forced shutdown on a real UPS
  cuts output and restores it when mains returns, so a host with "power on
  after AC loss" boots by itself.
- [ ] FU3. caja (`~/world`, `hosts/caja/modules/ups.nix`): set the two
  options, drop the `override.battery.*.low` directives and their comment.

## Commit progress

- [x] C1. test(ups): prove killpower runs on the low-battery shutdown path
- [ ] C2. feat(ups)!: shut down early on battery with braid.ups.shutdownAfter and shutdownBelowCharge
- [ ] C3. feat(doctor): report an inactive UPS guard and an unenforceable charge floor
