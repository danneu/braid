---
intent: Capture the scope, safety contract, and config shape of `braid.ups.*` before implementation. Read before adding UPS support, NUT configuration, or power-loss handling.
---

# UPS Integration

Status: Active

> Principles:
> - [Resilient by default](../principles.md#1-resilient-by-default)
> - [Safe-by-construction operations](../principles.md#3-safe-by-construction-operations)
> - [Sane defaults](../principles.md#7-sane-defaults)
> - [NixOS-native](../principles.md#9-nixos-native)

## Context

A btrfs RAID1 pool tolerates clean shutdowns, but sudden power loss during active I/O -- especially during a long-running `btrfs replace`, `btrfs device remove`, or post-add/remove balance -- can leave the pool in a state that requires manual recovery. This is the same risk surface that [decision 019](019-inhibit-sleep.md) protects against for suspend/wake, but it cannot use the same control model. A sleep inhibitor actively blocks the operating system from suspending; braid cannot analogously block a UPS from running out of battery. The control model here is different: reject avoidable starts on battery up front, and prove journal recovery for the unavoidable mid-mutation case.

A UPS solves this only if the host cooperates. NUT (Network UPS Tools) is the standard Linux interface, and nixpkgs already provides a mature `power.ups` module that configures NUT declaratively -- units, users, udev rules, killpower handling. braid's job is not to reimplement that, but to layer opinionated policy on top so that enabling `braid.ups.enable = true` gives a home NAS three specific guarantees:

1. Orderly shutdown before battery exhaustion for ordinary mounted operation.
2. Preflight refusal to start pool-mutating commands while already on battery.
3. Live UPS state visible in `braid ups status` and the TUI; live UPS status is used for preflight safety and upsmon critical-state shutdown (normally `OB` + `LB` together, per `reference/nut/clients/upsmon.c:1404`).

The guarantees do not extend to "safe against any power loss." A UPS firing LB during a mutation that started on AC still interrupts that mutation. Recovery for that case falls to the existing journal + `braid recover` path, and must be proven per mutation class by VM tests before this decision flips to Active.

"Just alert the user on low battery" is insufficient for guarantee (1): a prolonged outage with nobody present would still exhaust the battery during an active mount. The host must power off before battery exhaustion, because [decision 018](018-systemd-lifecycle.md)'s teardown sequence (`braid-online.service` ExecStop -> btrfs umount -> luks close) needs a non-trivial window of live power to complete cleanly.

## Decision

### Scope: standalone, USB, single-host

v1 supports one NUT-compatible UPS connected over USB to the NAS, monitored by the NAS itself. Not supported as first-class:

- networked NUT (primary/secondary across multiple machines)
- serial `apcsmart`, `snmp-ups`, or other non-USB drivers
- multiple UPSes per host

An escape hatch (`driver = "..."`, `port = "..."`) exists for users whose UPS speaks a non-USB protocol, but braid does not guarantee correct behavior outside the USB path.

Rationale: USB UPSes cover the vast majority of home NAS deployments. Every non-USB topology adds configuration surface (network auth, SNMP community strings, serial port permissions) that braid would have to validate and test. Single-host standalone avoids the two-machine primary/secondary dance and its timing/credential complexity.

### Wrap `power.ups`, do not reimplement NUT

The braid module sets `power.ups.*` values from its higher-level options. It does not write `ups.conf`, `upsd.conf`, or `upsmon.conf` directly, and does not define its own `nut-*` systemd units.

This is a deliberate departure from the pattern in `modules/braid/fan-control.nix`, which owns its unit because nixpkgs' `hddfancontrol` module has concrete lifecycle bugs. The nixpkgs `power.ups` module has no equivalent known defect; reimplementing its surface would duplicate work and diverge over time.

### Data source: shell out to `upsc`

The TUI and `braid ups status` command read UPS state by invoking `upsc <name>` and parsing its key/value output. This matches every other braid parser (btrfs, cryptsetup, lsblk, smartctl, smartd, hddfancontrol).

A `parse_upsc` module in `cli/src/parse/` handles the parse, with stable and unstable golden fixtures in `cli/tests/fixtures/`. NUT (`networkupstools`) joins btrfs-progs, cryptsetup, and util-linux in the parser-critical toolchain (see [decision 010](010-toolchain-pinning.md) and the parser-compatibility section of AGENTS.md), with fixture refresh required on any nixpkgs bump that changes its pinned version.

Pinning is load-bearing. A new `braid.packages.networkupstools` option is added alongside the existing `btrfsProgs`, `cryptsetup`, and `utilLinux` pins, defaulted to nixos-25.11's `networkupstools`. The module uses this pin to configure the NUT package the `power.ups` service resolves (exact nixpkgs option name to confirm during implementation) and includes the same derivation in the CLI wrapper's PATH so that `upsc` invoked from `braid ups status` resolves to the tested version rather than whatever the host's system path provides. [Decision 010](010-toolchain-pinning.md) and [principle 10](../principles.md#10-pinned-toolchain) are updated in the same implementation to name NUT as parser-critical.

`ups.status` is parsed as a set of flags (`OL`, `OB`, `LB`, `CHRG`, `DISCHRG`, `RB`, ...), not an enum. Display severity is derived from the combination; unknown tokens are preserved in the parsed model so that new NUT statuses do not silently disappear.

`braid ups status` defaults to a curated human-readable summary and supports `--json` for the typed parsed model. Raw `upsc` passthrough is not exposed; users who want that can still run `upsc` directly.

### Shutdown-on-LB = `systemctl poweroff`

When NUT fires the low-battery (LB) event, upsmon runs `systemctl poweroff`. systemd's standard shutdown sequence then unwinds `braid-online.service` (decision 018), which closes the btrfs mount and LUKS mappers. The host powers off via normal means before the UPS exhausts its battery.

This is not "alert only." The host genuinely shuts down, because the only safe state during a prolonged power outage is off. An alert-only policy would require the user to react in time, which defeats the point of unattended operation.

### Reject pool-mutating commands on battery (preflight hygiene only)

When `braid.ups.enable = true`, `braid add`, `braid remove`, `braid remove-missing`, and `braid replace` query UPS status at preflight and refuse with a `Validation`-shaped error if the status set contains `OB` or `LB`. The check sits alongside the existing preflight checks, before any journal write.

This is preflight hygiene, not a mutation-window guarantee. It narrows the surface that journal recovery must cover by rejecting the easy case -- "user starts `braid replace` while the power is already out" -- but it cannot and does not prevent LB from firing mid-mutation on work that started on AC. Mid-mutation power loss is handled by the existing journal + `braid recover` path; see the recovery-proof obligation in Open Questions.

This is not the power-side equivalent of decision 019's sleep inhibitor. A sleep inhibitor actively blocks suspend for the duration of the mutation window; braid cannot analogously block UPS-driven shutdown, because the UPS is dying and no amount of inhibiting changes that. Instead, the contract is: reject the avoidable case up front, and rely on recovery for the unavoidable case.

### Alert-model integration is deferred

Integrating UPS events into the shared `AlertState` / `AlertCause` model ([decision 014](014-alerts.md)) is deferred to a future ADR. Decision 014 guarantees "alerts stay latched until `braid ack`" -- the right shape for event-driven causes (disk errors, smartd), but wrong for live-state conditions like `OB` / `LB` (users expect those to clear when the UPS returns to `OL`). Reconciling that requires splitting `AlertCause` by persistence semantics (`LatchedUntilAck` vs. `ActiveWhileConditionHolds`) and updating `merge_into_latch`, `ack`, `status`, and the alert-test matrix. That is a core-invariant change that deserves its own ADR; smuggling it into UPS v1 would conflate two distinct concerns.

v1 therefore surfaces UPS state only through `braid ups status` and the TUI. Operators who are not actively watching those surfaces will not see on-battery or comms-loss conditions asynchronously in v1. This is a known gap until the follow-up ADR lands.

### `braid-online` becomes safety-critical under UPS

`modules/braid/braid-wrapper.sh` currently warns and exits successfully when `systemctl start braid-online.service` fails after a successful `unlock`/`add`/`recover`. Under `braid.ups.enable = true`, this silent-degradation path is unsafe: the user believes LB will trigger a clean shutdown, but without `braid-online.service` active, its ExecStop does not run and LUKS close is not guaranteed to complete before power dies.

`braid doctor` and the TUI flag "pool mounted but `braid-online` inactive" as a high-severity configuration fault whenever `braid.ups.enable = true`. The wrapper's warn-and-continue behavior otherwise remains unchanged; the UPS path adds a new detector, it does not change the underlying unlock sequence.

### Upsmon credential lifecycle

NUT requires upsmon to authenticate to upsd even in single-host standalone mode. The credential lives at `/var/lib/braid/upsmon.pass` with mode `0600`, owned by root, outside the Nix store.

Generation: a oneshot `braid-ups-secrets.service` creates the file if absent with a random token (e.g. `head -c 24 /dev/urandom | base64`) and exits. The oneshot is wired with `before = [ "upsd.service" "upsmon.service" ]` and `requiredBy = [ "upsd.service" "upsmon.service" ]` (the actual nixpkgs `power.ups` unit names), so upsd and upsmon hard-fail to start if secret creation fails rather than racing it. `systemd.tmpfiles` rules ensure `/var/lib/braid/` exists with correct ownership before the oneshot runs. The file is stable across rebuilds; regeneration happens only on explicit deletion. No rotation is performed because the scope is loopback upsmon<->upsd on a single host.

Reference: the rendered NUT configs consume the file via `power.ups.users.<name>.passwordFile` and `power.ups.upsmon.monitor.<name>.passwordFile` (not inline passwords), so the token never enters the Nix store or `nix-store --query` output.

## Proposed config surface

```nix
braid.ups = {
  enable = true;
  name = "ups";               # identifier used by upsd and upsc
  driver = "usbhid-ups";      # USB default; covers the vast majority of UPSes
  port = "auto";              # usbhid-ups's standard "find the device" value
};
```

Defaults applied internally, not surfaced as options in v1:

- standalone mode (upsd + upsmon on the same host, no network monitors)
- `SHUTDOWNCMD = systemctl poweroff`
- upsmon credentials per "Upsmon credential lifecycle"

Note: `NOTIFYCMD` is intentionally not configured in v1 -- alert-model integration is deferred (see "Alert-model integration is deferred" above).

The configured `name` is also written to `/etc/braid/config.json` so that `braid ups status` and the TUI do not have to guess which UPS to query.

## Deferred

- networked NUT (primary/secondary across hosts)
- non-USB drivers as first-class support (work via escape hatch, not tested)
- pre-shutdown grace window with `braid ups abort-shutdown`
- battery-age reminders driven by `battery.mfr.date` + the `RB` status flag
- multi-UPS per host
- UPS-triggered automatic pause of running scrub/balance (they resume on remount; acceptable as v1 behavior)

## Resolved questions

Each of these blocked the flip from `Draft` to `Active`. All three are now closed by VM tests committed in `tests/module/`.

1. **Recovery-proof for mid-mutation power loss (primary blocker).** Resolved by the four VM tests in `plans/wip/forced-shutdown-recovery-proof.md`'s matrix: `ups-lb-during-replace`, `ups-lb-during-remove`, `ups-lb-during-remove-missing`, and `ups-lb-during-balanced-add`. Each fires `OB LB` via `upsrw` while a different mutation class is in flight, lets `systemctl poweroff` run, reboots the VM, runs `braid recover`, and asserts the post-recover state matches what the original mutation would have produced -- including no orphaned LUKS mappers, no `MISSING` btrfs entries, no remaining single-profile chunks where RAID1 was intended, and a cleared `pending-op.json`. The Pre-M11 audit also surfaced two `cli/src/recover.rs` gaps that the same plan landed before the matrix ran: `pool_resize_device` is now replayed for `OpKind::Replace`, and a soft RAID1 balance is replayed for `OpKind::Add`, `OpKind::RemoveMissing`, and `OpKind::Replace` so single-profile chunks left by a cancelled mid-flight balance get drained before the journal is cleared.
2. **Shutdown ordering for ordinary mounted operation.** Resolved by `tests/module/ups-lb-clean-shutdown.{nix,py}` (Plan 1's M7). The VM test mounts an idle pool, fires `OB LB` via `upsrw`, and asserts `braid-online.service`'s ExecStop completes (and is not killed by `TimeoutStopSec`) before poweroff. The default `TimeoutStopSec = 5min` is sufficient for a single-disk pool; larger pools should retain that headroom.
3. **Battery-low threshold.** Resolved with the upstream NUT default. Plan 1's M7 (`ups-lb-clean-shutdown`) passed without raising `battery.runtime.low` from its driver-dependent default (often 120s). That test deliberately imports `tests/module/lib/ups-fixture.nix` with `upsmonTimings = null` so upsmon runs at upstream POLLFREQ/POLLFREQALERT/FINALDELAY = 5/5/5 -- the runtime-budget claim is therefore backed by representative timings, not the squeezed 1/1/0 cadence the Plan 3 matrix tests use to keep the LB-detection window narrower than an in-flight mutation. Larger real-world pools that risk exceeding the default budget can override `power.ups.upsmon.settings` (or the driver's `battery.runtime.low`) at the deployment level; braid does not need a dedicated option for v1.

## Consequences

- enabling UPS support is one line of Nix, plus two optional strings for non-default drivers
- for ordinary mounted operation, the host powers off cleanly on low battery without user intervention
- pool-mutating commands refuse to start while on battery, narrowing the journal-recovery surface to the mid-mutation case
- mid-mutation power loss is a supported recovery case, not a guarantee: `braid recover` is load-bearing for `replace` / `remove` / `remove-missing` / balanced `add` interrupted by LB-driven shutdown, and VM tests prove that coverage
- live UPS state is visible in `braid ups status` and the TUI; users not actively watching those surfaces do not get asynchronous notifications in v1 (alert-model integration deferred to a future ADR)
- NUT joins btrfs-progs, cryptsetup, and util-linux as a pinned parser-critical tool; nixpkgs bumps touching `networkupstools` trigger the same fixture-refresh obligation as the other three
- the existing `braid-online.service` lifecycle (decision 018) is load-bearing under UPS; its failure mode is no longer acceptable silent degradation and `braid doctor` reflects that
