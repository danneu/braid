[← braid](../index.md)

# UPS

This guide covers enabling UPS (uninterruptible power supply) support on a
braid NAS via NUT ([Network UPS Tools](https://networkupstools.org/)).

Enabling UPS support (`braid.enable = true` plus `braid.ups.enable = true`)
turns on three behaviors:

- **Orderly poweroff in a long outage.** After `shutdownAfter` on battery,
  or once the charge drops below `shutdownBelowCharge` on battery, braid
  shuts the host down through NUT. The UPS's own low-battery signal still shuts it down if it
  comes first. systemd unwinds `braid-online.service`'s ExecStop, which runs
  `braid lock` and cleanly unmounts the pool while the battery still has
  charge to spare. See [Shutdown policy](#shutdown-policy).
- **Preflight refusal without verified utility power.** `braid add` /
  `remove` / `remove-missing` / `replace` check UPS state at startup and
  refuse to begin a pool mutation unless the UPS reports verified utility
  power (`OL`). This narrows the surface that journal recovery needs to
  cover.
- **Live state visibility.** `braid ups status` (and the TUI Data tab) show
  the parsed `upsc` output: status flags, battery charge, runtime remaining,
  load, estimated watts, input voltage, and device info.

## Scope

v1 supports a single USB-connected UPS on the NAS, monitored by the NAS
itself (single-host standalone). Non-USB drivers work through the escape
hatch but are not tested.

## Minimal config

```nix
# configuration.nix
{
  braid = {
    enable = true;
    ups.enable = true;
  };
}
```

Defaults: `name = "ups"`, `driver = "usbhid-ups"`, `port = "auto"`,
`shutdownAfter = 300`, `shutdownBelowCharge = 50`. Rebuild and plug the UPS's
USB cable in; NUT's auto-detect finds the device.

Override the driver or port for non-USB UPSes:

```nix
braid = {
  enable = true;

  ups = {
    enable = true;
    name = "myups";
    driver = "apcsmart";
    port = "/dev/ttyS0";
  };
};
```

## Checking status

```sh
# Curated human summary
sudo braid ups status

# Machine-readable JSON (stable shape for scripts)
sudo braid ups status --json | jq .
```

Example human output:

```
UPS: ups
Status: OL  [ok] on utility power
Battery: 100%
Runtime: 30m 0s
Load: 17% (56 W estimated)
Input: 120.0 V (transfer 88-142 V)
Device: APC Back-UPS ES 550G
Battery manufactured: 2023/04/12
Last test: Done and passed
```

The human `Status:` line preserves the raw NUT tokens and adds the same
severity model used by the TUI UPS panel: `[ok]`, `[warn]`, `[fail]`, or
`[skip]`. Tags are colored on an interactive TTY and plain under `NO_COLOR` or
when output is piped.

The watts line is labeled `estimated` and omitted entirely if the UPS
does not report both `ups.load` and `ups.realpower.nominal`.

The `--json` output serializes the full parsed model plus braid's
`severity` verdict, from the same classifier the
[mutation preflight](#mutation-refusal-when-utility-power-is-not-verified)
uses. Only `"severity": "online"` proves utility power; the
[`ups status` page](../commands/ups-status.md#json-output) lists every
value. Distinct error sentinels are emitted for the common non-OK cases:

| Condition | JSON shape | Exit code |
| --- | --- | --- |
| UPS reachable | serialized `UpscOutput` plus `severity` | 0 |
| UPS query failed | `{"error": "query_failed", "detail": "exit <code>: <stderr>"}`. If stderr is blank, `detail` is just `exit <code>`. | 1 |
| UPS invocation failed (upsc could not run -- missing on PATH, killed by signal, or other runner-level failure) | `{"error": "invocation_failed", "detail": "command failed: upsc ups: <reason>"}`. When upsc is missing, `detail` ends with ` -- is braid.packages.nut on PATH?`. Other failures, such as a signal kill, get no hint. | 1 |
| UPS not enabled | `{"error": "ups_not_enabled"}` | 0 |

## TUI UPS panel

`braid tui`'s Data tab gains a UPS row when UPS support is enabled.
Status text is color-coded by the shared UPS severity model:

- **Green** -- OL (on utility power).
- **Yellow** -- OB (on battery, not yet critical).
- **Red** -- LB / TESTFAIL / COMMBAD / FSD (critical; shutdown imminent
  or comms-loss).
- **DarkGray** -- UPS query failed, or no UPS state available yet.

The **Shutdown** column shows the [shutdown policy](#shutdown-policy) as
`braid-ups-guard` sees it. On utility power it shows the armed policy
dimmed, such as `after 5m 0s or <50%`. During an outage it counts down
(`in 3m 12s or <50%`), turning red in the last minute. It shows
`guard down` in red when a trigger is set but the guard is not running,
`overdue` when the timer passed its deadline and no forced shutdown
followed, and `off` when both options are `null`. The countdown comes from
the guard's own clock, so it matches what the guard will do.

During an outage a banner above every tab says how long the UPS has been
on battery and when braid shuts down, or why it will not shut down early.
It clears once power returns.

The panel polls on the same 5-second cadence as the
[TUI fans panel](fan-control.md#tui-fans-panel). Press `r` to refresh both
pool and UPS probes immediately.

## Shutdown policy

braid shuts the host down at the first of three moments:

1. **On battery for `shutdownAfter` seconds** (default 300).
2. **On battery with `battery.charge` below `shutdownBelowCharge` percent**
   (default 50).
3. **The UPS reports low battery (`OB LB`)** -- the firmware's own
   last-resort signal.

```nix
braid.ups = {
  enable = true;
  shutdownAfter = 300;      # seconds on battery; null turns the timer off
  shutdownBelowCharge = 50; # percent; null turns the floor off
};
```

The timer lets short outages ride through. The charge floor covers an aging
battery or a grown load that drains faster than the timer assumes. Low
battery is the backstop, and fires late: often with about two minutes of
estimated runtime left.

### Sizing `shutdownAfter`

Measure the UPS's runtime at the NAS's real load (`braid ups status` shows
`Runtime` while on utility power), then:

```
shutdownAfter <= runtime x 0.5 - clean-shutdown time - 60 s margin
```

Keeping about half the battery in reserve is kinder to it and leaves room
for an estimate that drifts as the battery ages. If the result is under
about two minutes, the UPS is too small for the load. Setting
`battery.runtime.low` or `battery.charge.low` in NUT does not shut down
earlier: NUT raises low battery from those values only with the driver's
`ignorelb` flag, which braid does not use because it ignores the UPS's own
low-battery signal.

### How it works

`braid-ups-guard.service` polls the UPS every few seconds. The clock starts
on the first poll that shows `OB` and stops only when the UPS reports
utility power (`OL`); a failed query or an unrecognized status leaves it
running. A guard restart, such as a `nixos-rebuild switch`, keeps the
outage's original deadline, and time spent suspended counts.

When a trigger holds, the guard logs one line naming the reason and runs
`upsmon -c fsd`. From there the shutdown is the same as on low battery:

1. `upsmon` sets forced shutdown (`FSD`) on the UPS, writes
   `/run/killpower`, and runs `systemctl poweroff`.
2. systemd stops `braid-online.service`, whose ExecStop runs `braid lock`,
   which unmounts the pool and closes LUKS.
3. Late in shutdown, `ups-killpower.service` tells the UPS to cut its
   output and restore it when mains returns, so a host set to power on
   after AC loss boots by itself.

```sh
# Why did the NAS shut down? (previous boot)
journalctl -b -1 -u braid-ups-guard
```

If mains returns while the host is shutting down, a UPS that ignores
`shutdown.return` once on utility power may leave the host off until you
power it on.

### Turning it off

Set both options to `null` to shut down only on low battery, as before;
the guard is then not installed. Under the default upsmon timings
(`POLLFREQ = POLLFREQALERT = 5`, `FINALDELAY = 5`) the window between low
battery and poweroff is about 10 seconds plus however long `braid lock`
takes.

## Mutation refusal when utility power is not verified

With UPS enabled, `braid add` / `remove` / `remove-missing` / `replace`
refuse to start unless `upsc` returns a non-empty status set that
contains `OL` and no known blocker. The refusal cases are:

- on-battery (`OB`)
- a critical flag the TUI paints red (`LB` / `TESTFAIL` / `COMMBAD` /
  `FSD`)
- `OL` missing from an otherwise non-blocking status set
- `upsc` query or invocation failure (stopped daemon, unknown UPS name,
  or another fatal NUT error -- the message includes `upsc`'s stderr when
  present and omits that clause when stderr is blank)
- an empty or missing `ups.status`

Known non-critical advisory states such as `OL RB`, and unknown tokens
co-present with `OL` and no known blocker, still pass: the `OL` flag is
the affirmative utility-power proof, not a guarantee of full battery
health. Example refusal:

```
$ sudo braid add newdisk=/dev/disk/by-id/ata-TOSHIBA_NEW
error: cannot verify UPS is on utility power (UPS reports on-battery)
-- refusing to start add. Check 'braid ups status', restore utility
power, then retry.
```

Recovery: run `braid ups status` to confirm, fix the UPS/NUT state,
restore utility power, wait for the status to return to a trusted `OL`,
and retry the command.

Two clarifications:

- `braid doctor`'s `ups_daemon: ok` means the configured NUT daemon is
  reachable; it is not a guarantee that mutating-command preflight will
  pass. The refusal error from `add` / `remove` / `remove-missing` /
  `replace` is the primary channel for the exact mutation-readiness
  blocker.
- The `OL` gate assumes the configured NUT driver reports `OL` on utility
  power as documented by NUT. If a device or driver violates that
  contract, inspect with `braid ups status`; the recovery is to fix the
  NUT driver/config or disable `braid.ups` until the UPS state can be
  trusted.

## doctor checks

`braid doctor` adds these UPS-adjacent checks when UPS support is enabled:

- **ups daemon** -- fails if `upsc` is missing or cannot be spawned,
  because braid cannot verify the enabled UPS shutdown path. It warns
  if `upsc` runs but cannot reach the daemon or exits non-zero. Fix
  missing `upsc` by checking the braid wrapper/NUT package path; fix
  daemon reachability with `systemctl status upsd.service`.
- **braid-online** -- fails (high severity) if the pool is mounted but
  `braid-online.service` is not active. Without that service active,
  the UPS shutdown path does not unmount the pool, and the safety
  guarantee silently breaks. Fix by running `systemctl start
  braid-online.service` or `braid unlock`.
- **ups guard** -- fails if `braid-ups-guard.service` is not active
  while `shutdownAfter` or `shutdownBelowCharge` is set. Without the
  guard, early shutdown is off and only the UPS's own low-battery
  signal shuts the host down. Fix with `journalctl -u braid-ups-guard`
  and `systemctl start braid-ups-guard.service`. Skipped when both
  options are null.
- **ups floor** -- warns if `shutdownBelowCharge` is set but the UPS
  does not report `battery.charge`, so the charge floor can never fire
  and only the timer and low battery remain. Check `braid ups status`;
  if the UPS cannot report charge, set `shutdownBelowCharge = null`
  and rely on `shutdownAfter`. Skipped when the floor is null or the
  UPS query fails (the **ups daemon** check reports that).

Both checks skip when UPS support is disabled, and the braid-online check
additionally skips when the pool is not mounted (there is nothing for the
UPS shutdown path to unmount).

## v1 limitation: no async alert

There is no asynchronous notification when the UPS goes on battery or
loses comms in v1. Operators who are not actively watching `braid ups
status` or the TUI will not see those conditions -- only the orderly
shutdown (see [Shutdown policy](#shutdown-policy)) is automatic.

This is deliberate: integrating UPS events into braid's shared alert
model requires splitting `AlertCause` by persistence semantics
(latched-until-ack for disk errors, active-while-condition-holds for
live UPS states) and is out of scope for v1. See
[decisions/020-ups-integration.md](../design/decisions/020-ups-integration.md)
for the open-question status.

If you need asynchronous UPS notifications today, wire NUT's
`NOTIFYCMD` directly -- braid does not touch it.

## Related

- [ADR: UPS integration](../design/decisions/020-ups-integration.md) --
  scope, shutdown path, preflight contract.
- [`upsc` man page](https://networkupstools.org/docs/man/upsc.html) --
  the raw command braid parses.
- [`power.ups` NixOS options](https://search.nixos.org/options?query=power.ups) --
  the underlying nixpkgs module braid layers opinionated defaults on.
