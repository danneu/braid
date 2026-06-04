[← braid](../index.md)

# UPS

This guide covers enabling UPS (uninterruptible power supply) support on a
braid NAS via NUT ([Network UPS Tools](https://networkupstools.org/)).

Enabling UPS support (`braid.enable = true` plus `braid.ups.enable = true`)
turns on three behaviors:

- **Orderly poweroff on low battery.** When the UPS reports critical, NUT's
  `upsmon` invokes `systemctl poweroff`. systemd unwinds
  `braid-online.service`'s ExecStop, which runs `braid lock` and cleanly
  unmounts the pool before the battery exhausts.
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

Defaults: `name = "ups"`, `driver = "usbhid-ups"`, `port = "auto"`. Rebuild
and plug the UPS's USB cable in; NUT's auto-detect finds the device.

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
Status: OL
Battery: 100%
Runtime: 30m 0s
Load: 17% (56 W estimated)
Input: 120.0 V (transfer 88-142 V)
Device: APC Back-UPS ES 550G
Battery manufactured: 2023/04/12
Last test: Done and passed
```

The watts line is labeled `estimated` and omitted entirely if the UPS
does not report both `ups.load` and `ups.realpower.nominal`.

The `--json` output serializes the full parsed model. Distinct error
sentinels are emitted for the common non-OK cases:

| Condition | JSON shape | Exit code |
| --- | --- | --- |
| UPS reachable with populated `ups.status` | serialized `UpscOutput` | 0 |
| UPS reachable but `ups.status` empty | serialized `UpscOutput` plus `"warning": "ups_status_empty"` | 0 |
| UPS query failed | `{"error": "query_failed", "detail": "exit <code>: <stderr>"}` | 1 |
| UPS invocation failed (upsc could not run -- missing on PATH, killed by signal, or other runner-level failure) | `{"error": "invocation_failed", "detail": "command failed: upsc ups: <reason>"}` | 1 |
| UPS not enabled | `{"error": "ups_not_enabled"}` | 0 |

## TUI UPS panel

`braid tui`'s Data tab gains a UPS row when UPS support is enabled.
Status text is color-coded by severity:

- **Green** -- OL (on utility power).
- **Yellow** -- OB (on battery, not yet critical).
- **Red** -- LB / TESTFAIL / COMMBAD / FSD (critical; shutdown imminent
  or comms-loss).
- **DarkGray** -- UPS query failed, or no UPS state available yet.

The panel polls on the same 5-second cadence as the
[TUI fans panel](fan-control.md#tui-fans-panel). Press `r` to refresh both
pool and UPS probes immediately.

## What happens on low battery

1. `upsmon` sees `ups.status: OB LB`, declares the UPS critical.
2. `upsmon` runs its configured `SHUTDOWNCMD`. braid overrides the
   nixpkgs default (`shutdown now`) with `systemctl poweroff`.
3. systemd walks its shutdown sequence. `braid-online.service` stops
   with ExecStop running `braid lock`, which unmounts the pool and
   closes LUKS.
4. The host powers off before the battery exhausts.

Under the default upsmon timings (`POLLFREQ = POLLFREQALERT = 5`,
`FINALDELAY = 5`) the window between LB detection and poweroff is
~10 seconds, plus however long `braid lock` takes. The default
`battery.runtime.low` in most UPS drivers is around 120 seconds, which
is enough headroom for a single-disk pool's clean teardown. Larger
pools may need a wider `battery.runtime.low` (set at the NUT level,
not through braid).

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
  it exits non-zero)
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

`braid doctor` adds two UPS-adjacent checks when UPS support is enabled:

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

Both checks skip when UPS support is disabled, and the braid-online check
additionally skips when the pool is not mounted (there is nothing for the
UPS shutdown path to unmount).

## v1 limitation: no async alert

There is no asynchronous notification when the UPS goes on battery or
loses comms in v1. Operators who are not actively watching `braid ups
status` or the TUI will not see those conditions -- only the orderly
shutdown on LB is automatic.

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
