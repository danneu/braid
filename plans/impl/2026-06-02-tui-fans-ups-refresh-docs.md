# Plan: Document TUI Fans/UPS telemetry and fix the refresh model

## Context

`docs/commands/tui.md` (the `braid tui` command reference) is out of sync with
what the TUI renders and how it refreshes:

- The Data tab renders a **Fans** row and a **UPS** row when those subsystems
  are configured (`cli/src/tui/view/mod.rs#view_data`, gated on
  `model.fan_control.is_some()` / `model.ups_config.is_some()`), but tui.md
  documents neither. README.md's "Dashboard" bullet already advertises
  "chassis fan telemetry plus UPS state," so only the reference lags.
- The intro claims the dashboard "Refreshes on demand," which is wrong. The
  real model has two cadences:
  - Pool/disk/scrub/alert data re-probes **only on `r`**
    (`cli/src/tui/app.rs#update`, `Message::RefreshPool` -> `Effect::ProbePool`).
  - Fan and UPS telemetry **auto-poll every 5s** (`cli/src/tui/effect.rs`
    `FAN_PROBE_INTERVAL`/`UPS_PROBE_INTERVAL`; self-rescheduling loops in
    `app.rs#update` `RefreshFan`/`RefreshUps`) **and** also re-probe on `r`
    (the `RefreshPool` handler refreshes all three).
  - The screen redraws every 10s when idle so relative times stay current
    (`cli/src/tui/mod.rs#run_loop`, `IDLE_REDRAW_INTERVAL`), independent of
    data probes.
- `docs/guides/ups.md` already has a "## TUI UPS panel" section and references
  "the fan panel" -- a panel documented nowhere. Dangling reference.

**Outcome:** tui.md accurately documents both telemetry rows and the
dual-cadence refresh model; the fan panel gets a documented, symmetric home in
its guide; ups.md's dangling reference becomes a real link.

## Approach (Thorough + guide sync)

Reuse, don't duplicate. tui.md is the visual reference (what columns appear);
each domain guide owns interpretation (UPS color severity, fan curve meaning),
mirroring the existing ups.md precedent. tui.md links out rather than copying.

Vocabulary must match existing docs: "Battery"/"Runtime"/"Load"
(`docs/commands/ups-status.md`), "curve"/"driving" (`docs/guides/fan-control.md`),
"daemon" status. Use `--` not em-dash; ASCII per project style.

### 1. `docs/commands/tui.md` (primary)

- **Intro (line ~5):** drop "Refreshes on demand." Leave a tight one-liner and
  defer the model to a short note below, e.g.: "Interactive terminal dashboard
  showing pool state, disk health, allocation, scrub status, and active alerts."
- **New short "Refreshing" note** (under "What it shows"):
  - Pool, disk, and scrub data refresh on demand -- press `r`.
  - When enabled, Fans and UPS telemetry also refresh automatically every 5
    seconds (and immediately on `r`).
  - The `Reload: r` footer shows a spinner during the pool refresh and that
    probe's last duration in ms when idle; the automatic 5s Fans/UPS polls do
    not touch it (footer reads `pool_spinner_active` + `probe_duration`, both
    pool-only -- `probe_duration` is set solely by `PoolProbeFinished`).
  - One clause: the view redraws periodically so relative ("ago") times stay
    current.
- **New "Fans" entry** (parallel bullet to "Disk table"), marked "(when fan
  control is enabled)". The section *header* carries a `daemon:` status
  annotation (rendered by `view/mod.rs#section_block_with_status`; states from
  `#daemon_status_display`: active=green, activating/inactive=yellow,
  failed=red, unknown=gray) -- this is a header annotation, **not** a column.
  The data columns (`view/mod.rs#fan_section`) are PWM (raw/255 + %), RPM,
  Driving (hottest drive + temp), and Curve. Links to
  `[fan control guide](../guides/fan-control.md#tui-fans-panel)`.
- **New "UPS" entry**, marked "(when UPS support is enabled)". Same `daemon:`
  header annotation as Fans (same five states). The data columns
  (`view/mod.rs#ups_section`) are Status (color-coded flags), Battery, Runtime,
  and Load. Links to `[UPS guide](../guides/ups.md#tui-ups-panel)` for the
  Status color severity.
- **Data tab bullet (line ~66):** append "plus Fans and UPS rows when enabled."

### 2. `docs/guides/fan-control.md` (new section)

- Add "## TUI fans panel" mirroring ups.md's "## TUI UPS panel": one short
  paragraph on the Data-tab Fans row -- daemon status
  (`hddfancontrol-braid.service`), PWM/RPM, the Driving column (hottest drive
  setting the curve), the Curve column -- noting the 5-second poll cadence and
  `r` to refresh. This is the symmetric home ups.md's "fan panel" points to.

### 3. `docs/guides/ups.md` (one-line fix)

- Turn the dangling prose "the same 5-second cadence as the fan panel" into a
  real cross-link to `fan-control.md#tui-fans-panel`.

## Ground-truth references (read-only, do not edit)

- `cli/src/tui/view/mod.rs#fan_section`, `#ups_section`,
  `#section_block_with_status` (daemon indicator), `#view_data` (gating, footer).
- `cli/src/tui/app.rs#update` -- `RefreshPool` (re-probes all three),
  `RefreshFan`/`RefreshUps` (5s loops).
- `cli/src/tui/effect.rs` -- `FAN_PROBE_INTERVAL`/`UPS_PROBE_INTERVAL` = 5s.
- `cli/src/tui/mod.rs#run_loop` -- `IDLE_REDRAW_INTERVAL` = 10s.

## Out of scope

- No code changes. README.md is already accurate -- leave it.
- `--demo` does not show Fans/UPS (`fan_control`/`ups_config` are `None` in
  `model.rs#Model::new_demo`); do not imply demo mode shows them. The "(when
  enabled)" wording already covers this.
- No structural rewrite of "What it shows"; insert the new entries cleanly.

## Verification

- `mdbook build docs` -- must pass `mdbook-linkcheck2`, but scope expectations
  to what it actually guards: each link's target *file* resolves, is included in
  `SUMMARY.md`, and does not escape the book root. It does **not** validate
  `#fragment` anchors -- `linkcheck2` skips fragment resolution (logs that
  "fragment resolution isn't implemented";
  [linkcheck issue #3](https://github.com/Michael-F-Bryan/linkcheck/issues/3)),
  so a typo'd or later-renamed `#tui-fans-panel` / `#tui-ups-panel` passes CI
  silently -- the exact dangling-reference class this plan fixes. Treat
  linkcheck as a file-level check only.
- Anchor check (manual -- the real anchor guard, since linkcheck can't do it):
  run `rg '^##' docs/guides/fan-control.md docs/guides/ups.md` after adding the
  new heading, and confirm every `#fragment` in the three new/edited links
  resolves to an existing heading's GitHub slug -- "TUI fans panel" ->
  `tui-fans-panel` (new in fan-control.md), "TUI UPS panel" -> `tui-ups-panel`
  (existing in ups.md).
- Proofread: every column named in tui.md matches the headers in `fan_section`
  (PWM/RPM/Driving/Curve) and `ups_section` (Status/Battery/Runtime/Load), and
  the `daemon:` annotation matches `daemon_status_display`'s five states.
- Cross-doc: re-read ups.md "TUI UPS panel" and the new fan-control.md "TUI fans
  panel" to confirm they are symmetric and agree on the 5s cadence.
- Optional live check: `braid tui --demo` shows no Fans/UPS rows (confirms the
  gating wording); on a configured host `braid tui` shows both rows and the
  footer `Reload: r (Xms)`.
