# TUI: show the UPS early-shutdown deadline

## Problem

`braid-ups-guard` (plan/2026-10-22-1122) shuts the host down after
`shutdownAfter` on battery or below `shutdownBelowCharge`. Nothing shows
the operator that policy or its countdown. During an outage the TUI's UPS
row says `OB` and nothing about when braid will shut down. When the guard
is not running, the TUI looks healthy while early shutdown is off. Only
`braid doctor` says so, and only when someone runs it.

## Desired outcome

The UPS section gains a `Shutdown` column. During an outage, a banner
appears at the top of every tab.

```
on battery 1m48s -- braid shuts down in 3m12s, or below 50% charge (now 78%)
Data  Scrub  Browse
 ...
 UPS ─────────────────────────────────────────────── upsd: active ─
 Status    Battery  Runtime  Load            Shutdown
 OB        78%      12m      17% (56 W est.) in 3m12s or <50%
```

On mains the column is quiet (`after 5m or <50%`, dim), and there is no
banner.

## Evidence for load-bearing premises

- The guard already persists the outage start: one CLOCK_BOOTTIME reading
  in `/run/braid-ups-guard/clock`, present only while it is timing an
  outage (`cli/src/ups_guard.rs#ClockFile`, `#CLOCK_PATH`). The file is
  0600 and owned by the upsmon user. The TUI runs as root
  (`cli/src/main.rs#needs_root`), so it can read the file.
- The policy is already in the TUI model: `model.ups_config` is the config
  `Ups`, which carries `shutdown` (`cli/src/config.rs#UpsShutdownPolicy`).
- Once upsmon sends FSD, upsd prefixes `FSD` to `ups.status` for every
  reader (`reference/nut/server/netget.c`, `reference/nut/server/netlist.c`).
  So "shutting down" is visible in the `upsc` reading the TUI already
  takes. The guard does not need to export its in-memory `fsd_sent`.
- The TUI redraws on every UPS probe, which runs every 5 s
  (`cli/src/tui/effect.rs#UPS_PROBE_INTERVAL`), and redraws at least every
  10 s when idle (`cli/src/tui/mod.rs#IDLE_REDRAW_INTERVAL`).

## Decision

- D1. One shared contract for the guard's state. The guard's clock file
  reader and writer (`ClockFile`, `CLOCK_PATH`) and the guard unit name are
  the only definitions. The guard, the TUI, and `braid doctor` all use them;
  the doctor's private copy of the unit name goes away. Because the TUI and
  the guard are the same binary, sharing the type is the cross-process
  contract plan/2026-10-22-1122/FU1 asked for. No new file format.
- D2. The UPS probe also reads, when a shutdown trigger is configured: the
  guard unit's ActiveState, the clock file, and CLOCK_BOOTTIME. It stores
  the time on battery so far, measured at the probe. The countdown advances
  only when a probe completes: about every 5 s normally, never while a
  probe is stuck (AR3).
- D3. The clock file, not the TUI's own reading of `ups.status`, decides
  whether a countdown is running. The guard already applies the "only OL
  stops the clock" rule (plan/2026-10-22-1122/I2). The TUI must not
  re-derive it.
- D4. One pure function maps (policy, guard unit state, clock reading, UPS
  reading, time on battery) to a display state. Both the column and the
  banner render from that state. States, highest priority first:

  | State | When | Column | Banner |
  | --- | --- | --- | --- |
  | shutting down | `FSD` in `ups.status`, whatever the policy or guard | `shutting down` (red) | `UPS forced shutdown -- braid is shutting down` (red) |
  | off | both triggers null | `off` (dim) | on battery only: `on battery -- early shutdown is off; braid shuts down at UPS low battery` (yellow) |
  | guard down | trigger set, guard unit not active or activating | `guard down` (red) | on battery only: `on battery and braid-ups-guard is not running -- only UPS low battery will shut down` (red) |
  | overdue | clock running, timer deadline passed by more than the guard's detection bound (plan/2026-10-22-1122/I8), no `FSD` | `overdue` (red) | `on battery Ns -- shutdown deadline passed; check journalctl -u braid-ups-guard` (red) |
  | counting | clock running | `in 3m12s or <50%` (yellow, red under 60 s) | `on battery 1m48s -- braid shuts down in 3m12s, or below 50% charge (now 78%)` (yellow, red under 60 s) |
  | starting | `ups.status` has `OB`, guard active, no clock yet | `timing...` (yellow) | `on battery -- braid-ups-guard is starting its shutdown clock` (yellow) |
  | clock error | clock file unreadable or corrupt | `clock error` (red) | none |
  | armed | none of the above | `after 5m or <50%` (dim) | none |

  Each trigger part renders only when that trigger is set. Timer only:
  `in 3m12s`. Floor only: `below 50%`, and the banner omits "in ...". When
  the charge is unreadable, the banner says `(charge unknown)`.
- D5. The banner is its own row. It sits below the pool alert and above
  the static advisories, because an outage is more urgent than a header
  backup reminder. It shows on every tab.
- D6. Durations use the TUI's existing duration formatter, with seconds,
  for example `3m12s`. All text is ASCII.

## Invariants

- I1. The TUI never shows a countdown unless the guard's clock file holds
  one. It never invents an outage start.
- I2. When early shutdown is configured and the guard is not running, both
  surfaces say so. They never show the armed policy as if it were in force.
- I3. The display is read-only. The TUI writes no guard state and starts or
  stops nothing.
- I4. Without UPS support, the demo included, the TUI is unchanged: no
  column and no banner.

## Proof obligations

- PO1 (D4, I1, I2). Unit tests over the pure display function cover every
  row of the D4 table, the priority order where two states hold (FSD beats
  everything, including off and guard down; guard down beats counting), each single-trigger variant, the
  under-60 s color change, and the unreadable charge.
- PO2 (D2, D3). Probe tests with MockRunner and a temporary clock path:
  - a guard unit state and a clock file become the snapshot's guard fields;
  - no clock file means no countdown;
  - a corrupt file means clock error;
  - no trigger configured means no guard probe at all (no systemctl request);
  - a failed `upsc` query with a running clock still yields the guard
    state and the countdown;
  - guard unit states classify as D4 requires: `activating` is not guard
    down, `deactivating` and `reloading` are.
- PO3 (D1). The guard writes and the TUI probe reads the clock through the
  same `ClockFile` (the existing round-trip test covers its format). The
  doctor check and the probe name the unit through the same constant.
- PO4 (D4, D5). Render snapshot tests of the Data tab: on mains (armed),
  counting (column plus banner), and guard down on battery. One snapshot of
  another tab shows the banner there too.

## Out of scope

- OOS1. `braid ups status` output. Same data, different surface; a
  follow-up can reuse the D4 state.
- OOS2. Predicting when the charge floor will trip. The plan shows charge
  against the floor; a projected time from the discharge rate or
  `battery.runtime` would be a guess.
- OOS3. Per-frame countdown ticking between probes.
- OOS4. Alerts or notifications for on-battery state. ADR 020 defers the
  alert model.

## Accepted risks

- AR1. The TUI and the guard poll independently. For up to one poll after
  power returns, the TUI can show `OL` with a countdown still running. D3
  makes this the honest view of the guard's state, and it clears on the
  guard's next poll.
- AR2. No VM test of the TUI rendering. The pieces that cross a process
  boundary are covered: the clock format by the shared `ClockFile`
  round-trip, and the unit state by the same ActiveState probe the TUI
  already uses for `upsd.service`. A pty VM test would cost more than the
  risk it covers.
- AR3. A stuck `upsc` freezes the TUI's whole UPS row, the new countdown
  included, until it returns. Every UPS field already behaves this way. The
  display is advisory, and the guard enforces the deadline on its own
  bounded poll (plan/2026-10-22-1122/I8), so a stale countdown cannot delay
  the shutdown.

## Implementation discretion

- Where the shared unit-name constant lives, and the exact fields added to
  `UpsSnapshot`.
- Column width and truncation at narrow terminals, as long as the 80-column
  layout keeps every column readable.
- Exact banner wording within the D4 meaning, as long as it stays ASCII and
  names the reason.

## Commit progress

- [x] C1. feat(tui): show the UPS early-shutdown deadline in a Shutdown column and an outage banner

## Implementation notes

- Durations render through `cli/src/util.rs#format_duration_secs` as D6
  requires, so the text reads `3m 12s` (with a space), not the mockup's
  `3m12s`.
- "Guard running" means ActiveState `active` or `activating`, matching the
  doctor check. The `starting` row uses the same test, so a guard mid-restart
  on battery with no clock yet shows `timing...`, not `armed`.
- The overdue bound is `cli/src/ups_guard.rs#TRIGGER_LAG_BOUND` (one poll
  interval plus one query wait), exported next to the constants it derives
  from rather than restated in the TUI.
- Banner wording for guard down and overdue was shortened so both fit in 80
  columns. The counting banner fits at 80 columns for outages under an hour;
  a longer one, or `(charge unknown)`, is cut off at the right edge.
