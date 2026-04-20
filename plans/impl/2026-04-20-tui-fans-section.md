# Plan: Add Fans section to TUI Data tab

## Context

`braid.fanControl` (hddfancontrol-driven HDD fan control) was added recently (commit `c6d07af`). The user currently monitors fan behavior in a separate terminal:

```
watch -n2 'cat /sys/devices/platform/f71882fg.656/fan2_input; sensors drivetemp-*'
journalctl -u hddfancontrol-braid -f
```

The TUI's Data tab already shows per-disk SMART temperatures with session min/max watermarks (`Temp` column, see `cli/src/tui/view/snapshots/snapshot_temperature_column.snap`). It does **not** show the fan side of the equation, so the user can't see the temp→fan relationship without leaving the TUI.

This plan adds a single-row "Fans" section between `Disks` and the bottom spacer on the Data tab — PWM, RPM, the hottest monitored drive ("Driving"), and the configured curve thresholds.

**Out of scope (deferred):** the response-curve viz (#3 from the design discussion), sparklines/history, multi-fan support (Nix module currently models one fan).

## Revisions from the prior plan (and why)

These corrections came from a plan review against `reference/hddfancontrol/src/`:

1. **Fan telemetry is independent of the pool**, not a `PoolState` field.
   - Reason: `hddfancontrol-braid` runs with `-d ata` (all visible SATA disks) regardless of LUKS/btrfs state (`modules/braid/fan-control.nix:145,188`). The chassis fan loop runs while the pool is locked. Hiding fan info when the pool is offline defeats the goal.

2. **"Driving" candidate set is a best-effort approximation of hddfancontrol's `-d ata` selector.**
   - Reason: hddfancontrol's `-d ata` (`reference/hddfancontrol/src/cl.rs:117-135`) enumerates `/dev/disk/by-id/`, filters to entries with the `ata-` prefix, **excludes** anything ending in `-partN`, then resolves those to drive paths. Drivetemp is then probed for that selected set (`reference/hddfancontrol/src/probe/drivetemp.rs:18-22`). Walking `/sys/block/sd*` would over-include (e.g. a non-`ata-*` USB-attached SSD that exposes drivetemp would falsely register as a Driving candidate).
   - **Approximation, not parity.** The TUI silently skips broken `ata-*` symlinks, non-`/dev/sdX`-shaped targets, and entries whose drivetemp probe fails. The daemon's actual setup logic differs in the failure modes: it may refuse to start or report errors that the TUI doesn't see. **Daemon health (revision 6) is the source of truth for whether the control loop is actually working** — the Driving column reflects "what the TUI thinks the daemon would pick", not "what the daemon actually picked".

3. **Curve string uses correct hddfancontrol semantics.**
   - Reason: in `reference/hddfancontrol/src/fan.rs:203-211,332-344`:
     - Steady-state PWM = `max_stop + (255 - max_stop) * speed_fraction`.
     - `speed_fraction = clamp((temp - min_temp)/(max_temp - min_temp), min_fan_speed_percent/100, 1.0)`.
     - `min_start` is **only** applied during fan startup-from-zero (within `STARTUP_DELAY` of the fan turning on).
   - Original plan rendered `PWM 70-255 (min 20%)` using `min_start` as the lower bound — that's wrong. Render in config-language (`30-40° -> 20-100%`) and include `max_stop` in the JSON so a future detail view can show derived steady-state PWM if desired.

4. **VM test asserts the generated `/etc/braid/config.json` shape**, not just the systemd unit (existing `tests/module/fan-control.py:15-77` only checks unit/udev wiring).

5. **Sysfs and dev paths take injectable roots end-to-end** — `sysfs_root: &Path`, `dev_root: &Path` (with `dev_by_id_root` derived as `dev_root.join("disk/by-id")`). Production passes `Path::new("/sys")` and `Path::new("/dev")`. Tests pass `tempfile::TempDir` paths. The canonicalization filter checks "target lives under `dev_root`" instead of hardcoding `/dev/`, so symlinks pointing at `<tmpdir>/dev/sda` validate cleanly under test.

6. **Daemon health (`systemctl is-active hddfancontrol-braid.service`) is part of the fan snapshot**, rendered in the section header.
   - Reason: without this, a failed/restarting daemon shows current sysfs PWM/RPM/drivetemp values that look healthy — the user loses information they had with `journalctl -u hddfancontrol-braid -f`. The control loop being live is the actual question; sensor values are secondary.

7. **Fan probe runs on its own cheap cadence; pool probe stays manual-only.**
   - Reason: the fan probe is sysfs reads + one `systemctl is-active` — sub-millisecond, sleep-friendly (drivetemp doesn't wake idle drives). The pool probe runs `smartctl -H -A` per disk plus btrfs/cryptsetup commands — heavy, can interfere with HDD spindown, and the user did not ask to change pool refresh behavior. Coupling the two would defeat the deliberate choice of `drivetemp` for cheap monitoring.
   - **Split the effects:** add `Effect::ProbeFan` separate from `Effect::ProbePool`. `Message::FanProbeFinished` re-schedules itself every `FAN_PROBE_INTERVAL` (5s); `Message::PoolProbeFinished` does **not** auto-reschedule (the `app.rs:160-164` TODO stays as-is). Initial `Model::new` fires both; manual `r` refreshes both; the background fan tick refreshes only fan state.

## Visual spec

Active (matches `snapshot_with_pool.snap` style; `section_block_with_status("Fans", &daemon_status)` extends the existing `section_block` at `cli/src/tui/view/mod.rs:92-98`). Section title shows daemon health; cell colors stay the existing palette unless daemon is unhealthy (then sensor cells render dim to signal "values are still real but the control loop isn't acting on them"):

```
 Fans -- daemon: active ─────────────────────────────────────────────
     PWM           RPM   Driving       Curve
   1 215/255 84%  1240   ironwolf 38°  30-40° -> 20-100%
```

Daemon failed (rendered in red; sensor row dimmed):

```
 Fans -- daemon: failed ─────────────────────────────────────────────
     PWM           RPM   Driving       Curve
   1 215/255 84%  1240   ironwolf 38°  30-40° -> 20-100%
```

Daemon activating/restarting (yellow):

```
 Fans -- daemon: activating ─────────────────────────────────────────
```

Pool offline, fan daemon still running:

```
 Fans -- daemon: active ─────────────────────────────────────────────
     PWM           RPM   Driving       Curve
   1 215/255 84%  1240   sdb 38°       30-40° -> 20-100%
```

(The Driving label is the friendly braid disk name when the drivetemp's sd* maps to a known pool member; otherwise the raw `sdX`.)

Sysfs PWM/RPM read failed:

```
   1 -/-           -     ironwolf 38°  30-40° -> 20-100%
```

No drivetemp readings at all (kernel module not loaded yet, no SATA disks):

```
   1 215/255 84%  1240   -             30-40° -> 20-100%
```

`systemctl is-active` itself errored (e.g. systemctl unavailable, command spawn failed):

```
 Fans -- daemon: unknown ────────────────────────────────────────────
```

When `fan_control` is absent from `/etc/braid/config.json` (`braid.fanControl.enable = false`): section omitted entirely (no header, no row, no layout space).

## Approach

**Config plumbing (mirrors Nix shape).** Extend `/etc/braid/config.json` (generated by `modules/braid/cli.nix:13-15`) with an optional `fan_control` object keyed 1:1 to `braid.fanControl`. Mirror the recent `pwm = { platformDevice; number; }` grouping (commit `a886f71`). Include `max_stop` (needed for any honest PWM calculation), exclude `interval` (daemon-only).

**Probe.** Add `read_fan_snapshot(runner: &impl CommandRunner, sysfs_root: &Path, dev_root: &Path, disk_by_id: &HashMap<String,String>, fan_control: &FanControl) -> FanSnapshot`. Three independent sub-reads, each best-effort:
- **Fan hardware** (`Option<FanReading>`): resolve hwmon glob via `<sysfs_root>/devices/platform/<dev>/hwmon/hwmon*/{device/,}pwm<N>` (same logic as `modules/braid/fan-control.nix:166-187`). Read `pwmN` (u8) and sibling `fanN_input` (u32). Either error → both `None` (they share a sysfs dir; failure modes are correlated).
- **Driving drive** (`Option<DrivingDrive>`): approximate hddfancontrol's `-d ata` selector for display purposes (see Revision 2 — daemon health is the source of truth for actual control-loop liveness):
  1. Enumerate entries in `<dev_root>/disk/by-id/` whose `file_name()` starts with `ata-` and does **not** match the `-partN` suffix (`reference/hddfancontrol/src/cl.rs:122-130`).
  2. Canonicalize each match (`std::fs::canonicalize`); accept the result only if it `starts_with(dev_root)` and its `file_name()` looks like `sdX` (alphabetic suffix). Otherwise skip the entry. The strip-prefix step yields the `sdX` label without hardcoding `/dev/`.
  3. For each resolved `sdX`, look up `<sysfs_root>/block/sdX/../../hwmon/hwmon*/` for a subdir whose `name` file equals `drivetemp`, then read sibling `temp1_input` (millidegrees → divide by 1000 → `i16` celsius). Mirrors `reference/hddfancontrol/src/probe/drivetemp.rs:20-46`.
  4. Pick the hottest; tie-break alphabetically by sdX.
  5. Map sdX → friendly braid name by canonicalizing each pool disk's `by_id` path through the **same `dev_root`-rooted** routine; fall back to raw `sdX`.
- **Daemon health** (`DaemonStatus`): run `systemctl is-active hddfancontrol-braid.service` via the existing `CommandRunner` abstraction (`cli/src/cmd.rs`). Add a new `CmdRequest::SystemctlIsActive { unit: String }` variant. Parse the one-word output:
  - `"active"` → `DaemonStatus::Active`
  - `"activating"` / `"reloading"` / `"deactivating"` → `DaemonStatus::Transitioning`
  - `"inactive"` → `DaemonStatus::Inactive`
  - `"failed"` → `DaemonStatus::Failed`
  - any other output, command spawn error, or non-zero exit not matching the above → `DaemonStatus::Unknown`
  - Note: `systemctl is-active` exits non-zero (3) when the unit is inactive/failed but still prints the status word — do **not** treat non-zero exit as `Unknown` blindly; parse stdout first.

**Render.** Single-row table styled like `pool_df_table` (`cli/src/tui/view/mod.rs:171-205`). Curve cell: `format!("{}-{}° -> {}-100%", min_temp, max_temp, min_fan_speed_percent)`. Section header is augmented with the daemon status (`Fans -- daemon: <state>`) using a new `section_block_with_status(title, status)` helper that mirrors `section_block` but appends a colored status span.

**Refresh cadence.** Two independent probes with separate cadences:

| Probe | Cadence | Triggered by |
| --- | --- | --- |
| `Effect::ProbePool` (existing) | Manual / on initial mount only | `Message::RefreshPool` (key `r`), `Model::new` |
| `Effect::ProbeFan` (new) | Auto every `FAN_PROBE_INTERVAL` (5s) **plus** on initial / manual refresh | `Model::new`, `Message::RefreshPool`, self-rescheduling on `Message::FanProbeFinished` |

Pool probe behavior is **unchanged** — the `app.rs:160-164` TODO stays a TODO. Fan probe is sub-millisecond and doesn't wake idle drives, so 5s polling is safe. `Effect::ScheduleFanProbe { delay }` mirrors the existing `Effect::ScheduleProbe` → `Event::PollRefresh` pattern at `effect.rs:58-64` — it sleeps and emits an event, nothing more. Full flow: `ScheduleFanProbe` → `Event::PollFanRefresh` → `Message::RefreshFan` (the only place the model guard runs). When the guard sees `fan_probe_inflight == true`, the handler **re-arms the schedule only** (returns `[ScheduleFanProbe]`) — never silently drops the loop and never spawns a duplicate probe. `model.fan_probe_inflight: bool` lives on `Model`.

**Why config.json over env vars:** project already does this (`cli/src/config.rs`); typed via serde; presence of the key is the enable signal; VM tests inject via `environment.etc."braid/config.json".text = builtins.toJSON {...}`.

## Files to modify

### Rust CLI

**`cli/src/config.rs`** — extend `Config` + `RawConfig`:
- Add `pub struct FanControl { pwm: Pwm, min_temp: u8, max_temp: u8, min_fan_speed_percent: u8 }` (Deserialize + Clone + PartialEq + Debug).
- Add `pub struct Pwm { platform_device: String, number: u8, min_start: u8, max_stop: u8 }` — calibration values (`min_start`, `max_stop`) live with the channel they describe.
- Add `pub fan_control: Option<FanControl>` to `Config` and `RawConfig`.
- Add `Config::fan_control(&self) -> Option<&FanControl>` accessor.
- Unit tests: parse with and without `fan_control`; reject malformed `pwm`; round-trip the example JSON shown below.

**`cli/src/tui/model.rs`** — add fan state at the model level (not on `PoolState`):
- `pub struct FanReading { pub pwm_raw: u8, pub rpm: u32 }`
- `pub struct DrivingDrive { pub label: String, pub celsius: i16 }`
- `pub enum DaemonStatus { Active, Transitioning, Inactive, Failed, Unknown }` — derived from `systemctl is-active`.
- `pub struct FanSnapshot { pub fan: Option<FanReading>, pub driving: Option<DrivingDrive>, pub daemon: DaemonStatus, pub probed_at: Instant }` — `daemon` is not `Option` because every probe attempt produces *some* status (worst case `Unknown`).
- Add `pub fan_control: Option<crate::config::FanControl>` to `Model` (line 172-189).
- Add `pub fan: Option<FanSnapshot>` to `Model` — overwritten on every probe attempt (success or all-None).
- Extend `Model::new` (line 193-226) and `Model::new_demo` (line 228-248) signatures with `fan_control: Option<FanControl>`. Initial `fan: None`.
- **Do not put `FanSnapshot` on `PoolState`.** Fan state outlives any single pool mount/unmount cycle.

**`cli/src/tui/mod.rs:47-48`** — pass through:
```rust
let (mut model, init_effects) = Model::new(
    disk_names, disk_by_id, config.mount_point().0.clone(),
    config.fan_control().cloned(),
    advisories, paths.clone());
```
Update `run_demo()` (line 57+) to pass `None`.

**`cli/src/tui/probe.rs`** — split into two probes called from the same Effect thread:
- Keep `probe_pool_for_tui` unchanged in signature; it already returns `Option<PoolState>`.
- Add `pub fn probe_fan_for_tui(runner: &impl CommandRunner, sysfs_root: &Path, dev_root: &Path, disk_by_id: &HashMap<String,String>, fan_control: &FanControl) -> FanSnapshot` (always returns a snapshot).
  - `execute_effect` calls this with `RealRunner`, `Path::new("/sys")`, `Path::new("/dev")`. `disk_by_id` maps canonicalized sdX → friendly braid name. `runner` is used for the `systemctl is-active` call.
- Internals split for testability — every function takes injected roots, no hardcoded `/sys` or `/dev`:
  - `fn resolve_pwm_dir(sysfs_root: &Path, fc: &FanControl) -> Option<PathBuf>` — runs the hwmon glob.
  - `fn read_fan_hardware(pwm_dir: &Path, n: u8) -> Option<FanReading>` — reads `pwmN` + `fanN_input`.
  - `fn enumerate_ata_drives(dev_root: &Path) -> Vec<String>` — reads `<dev_root>/disk/by-id/`, filters `ata-*` minus `*-partN`, canonicalizes, strips `dev_root` prefix, returns `["sda", "sdc", ...]` (already validated as `sdX`-shaped).
  - `fn read_drivetemp(sysfs_root: &Path, sd_name: &str) -> Option<i16>` — `block/<sdX>/../../hwmon/.../temp1_input`.
  - `fn map_disk_by_id_to_sd(dev_root: &Path, disk_by_id: &HashMap<String, String>) -> HashMap<String, String>` — canonicalize each pool member's by-id path; map `sdX → friendly_name` for entries that resolve under `dev_root`.
  - `fn pick_driving(temps: &[(String, i16)], sd_to_friendly: &HashMap<String, String>) -> Option<DrivingDrive>` — argmax + label mapping (alphabetical tie-break).
  - `fn probe_daemon_status<R: CommandRunner>(runner: &R, unit: &str) -> DaemonStatus` — runs `CmdRequest::SystemctlIsActive`, parses stdout (parsing rules in Approach above). Pure function over command output → easily unit-tested via `MockRunner`.

**`cli/src/cmd.rs`** — add a new command variant:
- `CmdRequest::SystemctlIsActive { unit: String }` → `CmdArgs { program: "systemctl", args: ["is-active", &unit] }`.
- `RealRunner` already handles non-zero exits by returning the output; no special-case needed (the parser inspects stdout regardless of exit code).
- Update `MockRunner` test scaffolding if required to register expected outputs for new probe tests.

**`cli/src/tui/effect.rs`** — add separate fan effects following the existing scheduler pattern (`Effect::ScheduleProbe` → `Event::PollRefresh` at line 58-64). Effects are payload carriers only; the model guard lives in the message handler:
- Add `Effect::ProbeFan { sysfs_root: PathBuf, dev_root: PathBuf, disk_by_id: HashMap<String,String>, fan_control: FanControl }`. `execute_effect` runs `probe_fan_for_tui` on a spawned thread and emits `Event::FanProbeFinished(FanSnapshot)`.
- Add `Effect::ScheduleFanProbe { delay: Duration }` — **carries no probe payload**. `execute_effect` spawns a thread that sleeps then emits `Event::PollFanRefresh`. The handler for the resulting `Message::RefreshFan` reads fresh values from `Model` and (if the guard passes) emits a fresh `Effect::ProbeFan`.
- Add `pub const FAN_PROBE_INTERVAL: Duration = Duration::from_secs(5);` alongside `PROBE_INTERVAL`.
- `Effect::ProbePool` is **unchanged**.

**`cli/src/tui/event.rs`** — add two new events (no rename):
- Add `Event::FanProbeFinished(FanSnapshot)` (probe completion).
- Add `Event::PollFanRefresh` (scheduler tick — empty payload). Mirrors `Event::PollRefresh` at line 25.
- Update `From<Event> for Message` (line 25, 45-46) to map: `FanProbeFinished` → `Message::FanProbeFinished`, `PollFanRefresh` → `Message::RefreshFan`.
- `Event::PoolProbeFinished` is **unchanged**.

**`cli/src/tui/app.rs`** — add fan messages + handlers (no rename of pool messages, no change to pool refresh behavior):
- Add `Message::FanProbeFinished(FanSnapshot)` and `Message::RefreshFan`.
- Add `model.fan_probe_inflight: bool` to `Model`.
- Handler for `Message::RefreshFan` (the scheduler's effective destination):
  1. If `model.fan_control.is_none()` → `vec![]` (defensive; loop should have stopped already).
  2. If `model.fan_probe_inflight` → re-arm only: `vec![Effect::ScheduleFanProbe { delay: FAN_PROBE_INTERVAL }]`. Avoids dropping the loop without spawning a duplicate probe.
  3. Otherwise: set `model.fan_probe_inflight = true;` and return `vec![Effect::ProbeFan { ..., fan_control: model.fan_control.clone().unwrap() }]`.
- Handler for `Message::FanProbeFinished(snapshot)`:
  1. `model.fan = Some(snapshot); model.fan_probe_inflight = false;`
  2. Return `vec![Effect::ScheduleFanProbe { delay: FAN_PROBE_INTERVAL }]` to keep the loop running.
- `Message::RefreshPool` (line 55-69) **also** emits `Message::RefreshFan` (or directly `Effect::ProbeFan` after running the same guard inline) so manual `r` refreshes both — but the pool effect path is unchanged.
- `Model::new` (model.rs:201-205) emits **both** `Effect::ProbePool` (existing) and (when fan_control is set) `Effect::ProbeFan` as initial effects, **and** initializes `fan_probe_inflight = fan_control.is_some()` so the model's view of in-flight matches the spawned thread. (Demo / disabled cases initialize to `false`.)
- The pool TODO at `app.rs:160-164` stays a TODO. `Message::PoolProbeFinished` does not auto-reschedule.

**`cli/src/tui/view/mod.rs`** — render the section:
- Add `fan_section(model: &Model) -> Table<'_>` near `pool_df_table` (line 171). Single header row + single data row. Column widths chosen to match existing snapshot widths.
- Add `section_block_with_status(title: &str, status_text: &str, status_color: Color) -> Block<'_>` near `section_block` (line 92). Mirrors `section_block` but appends ` -- daemon: <status>` in the title with `status_color`. (Existing `section_block` keeps working unchanged for Pool/Disks/Scrub/Sharing.)
- Helper `format_curve(fc: &FanControl) -> String` → `"{min_temp}-{max_temp}° -> {min_fan_speed_percent}-100%"`.
- Helper `format_pwm(reading: &Option<FanReading>) -> String` → `"{raw}/255 {pct}%"` or `"-/-"`.
- Helper `daemon_status_display(status: &DaemonStatus) -> (&'static str, Color)` → `(active, Green)`, `(activating, Yellow)`, `(inactive, Yellow)`, `(failed, Red)`, `(unknown, DarkGray)`.
- In `view_data` (line 629-707): when `model.fan_control.is_some()`, add a 4th constraint `Constraint::Length(3)` (border + header + row) between disks and the spacer; render `fan_section(model).block(section_block_with_status("Fans", status_text, status_color))` into it.
- When daemon is `Failed` or `Inactive`, render the sensor cells with `Style::default().fg(Color::DarkGray)` (dim) to signal "values are still real but the loop isn't acting".
- The Fans section renders **regardless of `model.pool` status** — fan state is independent. (Before the first probe completes, `model.fan = None` → render the row with all-`-` placeholders and `daemon: unknown` in the header.)

**Snapshot tests in `cli/src/tui/view/mod.rs:1052+`:**
- `sample_fan_control() -> FanControl` — `pwm: { platform_device: "f71882fg.656", number: 2, min_start: 70, max_stop: 60 }, min_temp: 30, max_temp: 40, min_fan_speed_percent: 20`.
- `sample_fan_snapshot_active() -> FanSnapshot` — fan + driving populated, `daemon: Active`.
- `sample_fan_snapshot_no_hardware() -> FanSnapshot` — `fan: None`, `driving: Some(...)`, `daemon: Active`.
- `sample_fan_snapshot_no_drives() -> FanSnapshot` — `fan: Some(...)`, `driving: None`, `daemon: Active`.
- `sample_fan_snapshot_daemon_failed() -> FanSnapshot` — fan + driving populated, `daemon: Failed`.
- New snapshot tests:
  - `snapshot_fans_section_active` — pool mounted + fan_control + fan snapshot all present + daemon active.
  - `snapshot_fans_section_pool_offline` — `PoolStatus::NotMounted` + fan_control + fan snapshot present (verifies fan section renders without pool).
  - `snapshot_fans_section_no_hardware` — fan_control set, hardware read failed.
  - `snapshot_fans_section_no_drives` — fan_control set, drivetemp returned nothing.
  - `snapshot_fans_section_daemon_failed` — fan_control set, sensors fine, daemon failed (verifies dim-on-failure rendering and red status header).
  - `snapshot_fans_section_pre_probe` — fan_control set, `model.fan = None` (initial state before first probe completes); header shows `daemon: unknown`.
  - `snapshot_fans_section_disabled` — `fan_control: None` → no Fans header in buffer.
- Update existing `Model::new_demo(...)` callsites to pass `fan_control: None` (no visual change to those snapshots; existing `.snap` files unchanged).

**App-loop unit tests in `cli/src/tui/app.rs`** (extends the existing `#[cfg(test)] mod tests` block):
- `fan_probe_finished_schedules_only_fan_refresh` — feed `Message::FanProbeFinished(sample)`; assert returned effects = `[Effect::ScheduleFanProbe { delay: FAN_PROBE_INTERVAL }]`. No `Effect::ProbePool`, no `Effect::ScheduleProbe`. Catches accidental coupling of fan completion to pool refresh.
- `pool_probe_finished_returns_no_effects` — feed `Message::PoolProbeFinished(sample)`; assert returned effects = `[]`. Locks in the "pool stays manual-only" invariant so a future contributor doesn't auto-reschedule it without intent.
- `refresh_fan_skips_when_inflight` — set `model.fan_probe_inflight = true`, feed `Message::RefreshFan`; assert returned effects = `[Effect::ScheduleFanProbe { ... }]` (re-arm only, no `Effect::ProbeFan`).
- `refresh_fan_skips_when_disabled` — set `model.fan_control = None`, feed `Message::RefreshFan`; assert returned effects = `[]` (loop tears down cleanly).
- `refresh_fan_emits_probe_when_idle` — set `fan_control = Some(...)`, `fan_probe_inflight = false`, feed `Message::RefreshFan`; assert returned effects include `Effect::ProbeFan { ... }` and that `model.fan_probe_inflight` is now `true`.
- `refresh_pool_with_fan_idle_emits_both` — set `fan_control = Some(...)`, `fan_probe_inflight = false`, feed `Message::RefreshPool`; assert returned effects include **both** `Effect::ProbePool { ... }` (existing behavior) **and** `Effect::ProbeFan { ... }`, and that `model.fan_probe_inflight` flips to `true`. Pins manual `r` as a both-probe trigger.
- `refresh_pool_with_fan_inflight_emits_only_pool` — set `fan_control = Some(...)`, `fan_probe_inflight = true`, feed `Message::RefreshPool`; assert returned effects include `Effect::ProbePool` but **not** `Effect::ProbeFan` (no duplicate while existing probe is in flight). The auto-poll loop will catch up the fan separately.
- `refresh_pool_with_fan_disabled_emits_only_pool` — set `fan_control = None`, feed `Message::RefreshPool`; assert returned effects = `[Effect::ProbePool { ... }]` only.

**Probe unit tests in `cli/src/tui/probe.rs`** (using `tempfile::TempDir` for both `sysfs_root` and `dev_root`).

Fixture must be **symlink-backed** so the `block/<sdX>/../../hwmon` traversal lands in a per-drive directory (a flat `<tmp>/sys/block/sda/` dir would collapse to a shared `<tmp>/sys/hwmon`, hiding cross-drive resolution bugs):

```
<tmp>/dev/sda                                                    (regular file or empty file marker)
<tmp>/dev/sdb
<tmp>/dev/disk/by-id/ata-FOO       -> ../../sda                  (relative symlink)
<tmp>/dev/disk/by-id/ata-BAR       -> ../../sdb
<tmp>/dev/disk/by-id/ata-FOO-part1 -> ../../sda                  (excluded by selector)
<tmp>/sys/devices/pci/ata1/.../block/sda/                        (real dir)
<tmp>/sys/devices/pci/ata1/.../hwmon/hwmon0/{name,temp1_input}   (sibling of `block/`)
<tmp>/sys/devices/pci/ata2/.../block/sdb/
<tmp>/sys/devices/pci/ata2/.../hwmon/hwmon1/{name,temp1_input}
<tmp>/sys/block/sda                -> ../devices/pci/ata1/.../block/sda
<tmp>/sys/block/sdb                -> ../devices/pci/ata2/.../block/sdb
```

With this layout, `<tmp>/sys/block/sda/../../hwmon` resolves to `<tmp>/sys/devices/pci/ata1/.../hwmon` (sda-specific), and the same traversal from `sdb` lands in a different dir. A helper `build_sysfs_fixture(tmp: &Path, drives: &[(&str, i32)]) -> ()` should create this structure programmatically so each test can opt into the drives it needs.

Test cases:
- `resolve_pwm_dir` — `device/pwmN` layout, `pwmN` layout, 0 matches → `None`, 2 matches → `None`.
- `read_fan_hardware` — happy path, missing PWM file, missing fan_input file, non-numeric content.
- `enumerate_ata_drives` — picks `ata-WDC_*` and `ata-ST*`, **excludes** `ata-WDC_*-part1` and `ata-WDC_*-part2`, **excludes** `usb-*` and `nvme-*`, broken symlinks are skipped (don't poison the rest), targets pointing **outside** `dev_root` are skipped, non-`sdX`-shaped targets are skipped.
- `read_drivetemp` — multiple hwmon subdirs, only `name == "drivetemp"` is selected, missing `temp1_input` returns `None`, millidegrees → celsius conversion (`38500` → `38`).
- `read_drivetemp` (per-drive isolation) — fixture with two drives `sda` (38000 mC) and `sdb` (45000 mC) at distinct hwmon dirs; assert `read_drivetemp(root, "sda")` returns `Some(38)` and `read_drivetemp(root, "sdb")` returns `Some(45)`. This catches a flat-dir fixture or a wrong relative-traversal that would let both drives resolve to the same hwmon.
- `map_disk_by_id_to_sd` — pool by-id entries pointing inside `dev_root` map correctly; entries pointing outside are silently dropped.
- `pick_driving` — empty input → `None`, single entry, ties broken alphabetically, friendly-name mapping resolves pool members, unmapped sdX falls back to raw label.
- `probe_daemon_status` (using `MockRunner`):
  - stdout `"active\n"`, exit 0 → `Active`.
  - stdout `"activating\n"`, exit 0 → `Transitioning`.
  - stdout `"reloading\n"`, exit 0 → `Transitioning`.
  - stdout `"deactivating\n"`, exit 0 → `Transitioning`.
  - stdout `"inactive\n"`, exit 3 → `Inactive` (parses stdout despite non-zero exit).
  - stdout `"failed\n"`, exit 3 → `Failed`.
  - stdout `"unknown\n"`, exit 4 → `Unknown`.
  - command spawn error → `Unknown`.
  - empty/garbled stdout → `Unknown`.

### NixOS module

**`modules/braid/fan-control.nix`** — relocate calibration options:
- Move `minStart` and `maxStop` from top-level `braid.fanControl.{minStart,maxStop}` into the existing `pwm` group → `braid.fanControl.pwm.{minStart,maxStop}`.
- Update internal references (the `assertion` for `maxStop <= minStart` at lines 113-119 and the script's `-p "$pwm_path:${toString fc.minStart}:${toString fc.maxStop}"` at line 190).
- No backwards compatibility per project policy (`AGENTS.md` "No backwards compatibility").

**`tests/module/fan-control.nix`** (lines 31-32) and **`tests/module/fan-control-hotswap.nix`** (lines 43-44) — update fixture configs to the new shape:
```nix
fanControl = {
  enable = true;
  pwm = {
    platformDevice = "braid-test.0";
    number = 2;
    minStart = 65;     # moved from top level
    maxStop = 60;      # moved from top level
  };
  minTemp = 25;
  maxTemp = 45;
  minFanSpeedPercent = 10;
  interval = "20s";
};
```
Without these updates, the existing VM tests would fail at NixOS evaluation with "option does not exist".

**`manual/guides/fan-control.md`** and **`manual/guides/nixos-configuration.md`** — update example config snippets to use `pwm.minStart` / `pwm.maxStop` (search-and-replace; preserve surrounding prose).

**`modules/braid/cli.nix:13-15`** — conditional `fan_control` block (mirrors the new Nix shape exactly):

```nix
configFile = (pkgs.formats.json { }).generate "braid-config.json" ({
  mount_point = cfg.mountPoint;
} // lib.optionalAttrs cfg.fanControl.enable {
  fan_control = {
    pwm = {
      platform_device = cfg.fanControl.pwm.platformDevice;
      number = cfg.fanControl.pwm.number;
      min_start = cfg.fanControl.pwm.minStart;
      max_stop = cfg.fanControl.pwm.maxStop;
    };
    min_temp = cfg.fanControl.minTemp;
    max_temp = cfg.fanControl.maxTemp;
    min_fan_speed_percent = cfg.fanControl.minFanSpeedPercent;
  };
});
```

**`tests/module/fan-control.py`** — extend (existing test fixture in `tests/module/fan-control.nix` already enables `fanControl`; add a new subtest):

```python
with subtest("braid CLI config.json includes fan_control with correct shape"):
    cfg = json.loads(machine.succeed("cat /etc/braid/config.json"))
    assert cfg["mount_point"] == "/mnt/storage"
    fc = cfg["fan_control"]
    assert fc["pwm"]["platform_device"] == "braid-test.0"
    assert fc["pwm"]["number"] == 2
    assert fc["pwm"]["min_start"] == 65
    assert fc["pwm"]["max_stop"] == 60
    assert fc["min_temp"] == 25
    assert fc["max_temp"] == 45
    assert fc["min_fan_speed_percent"] == 10
    # interval is daemon-only -- must not appear in CLI config
    assert "interval" not in fc
```

(Add `import json` at the top of the file.)

Also add a complementary subtest for the disabled path:
- Construct a second VM node with `braid.fanControl.enable = false` (or extend the existing test to flip enable and re-check), assert `fan_control` key is absent from `config.json`. Easiest: separate small test case `tests/module/fan-control-disabled.nix` + `.py` mirroring the structure but with `fanControl.enable = false`.

## Example JSON shape

```json
{
  "mount_point": "/mnt/storage",
  "fan_control": {
    "pwm": {
      "platform_device": "f71882fg.656",
      "number": 2,
      "min_start": 70,
      "max_stop": 60
    },
    "min_temp": 30,
    "max_temp": 40,
    "min_fan_speed_percent": 20
  }
}
```

## Edge cases

| Case | Behavior |
| --- | --- |
| Hwmon glob matches 0 paths (kernel module not loaded) | `fan: None` → "-/- -" rendered |
| Hwmon glob matches >1 path | `fan: None` (defensive — same as systemd unit's check) |
| `pwmN` or `fanN_input` read errors | `fan: None` |
| `fanN_input` returns `0` (fan stopped / no tach) | Render `0` verbatim — that's the truth |
| No drivetemp hwmon found anywhere | `driving: None` → "-" rendered |
| `/dev/disk/by-id/ata-*` is empty (no ATA drives) | `driving: None` |
| `ata-*-partN` entries present | Skipped (matches `cl.rs:128` partition exclusion) |
| `ata-*` symlink target missing on canonicalize | Entry skipped, others still considered |
| Drivetemp drive doesn't map to any pool by-id | label = raw `sdX` |
| Multiple drives at same max temp | Alphabetical first (deterministic for snapshots) |
| `fan_control = None` (Nix option disabled) | Section omitted; layout unchanged |
| `pool` is Loading / NotMounted / Error | Fan section still renders (driving sourced from drivetemp, not pool) |
| Probe still in flight before first result | `model.fan = None` → row with all-`-` placeholders (briefly) |

## Verification

1. `just test-rust` — passes.
   - New `config.rs` deserialization tests (with/without `fan_control`).
   - New `tui/probe.rs` sysfs unit tests using `tempfile` (incl. per-drive isolation).
   - New `probe_daemon_status` tests via `MockRunner` covering all 5 status variants and error paths.
   - New `tui/app.rs` unit tests locking in fan/pool effect-loop separation (5 cases above).
   - 7 new snapshot tests in `tui/view/mod.rs`.
2. `cargo insta review` — accept the 7 new snapshots; existing snapshots unchanged.
3. `just test-vm fan-control` — extended VM test asserts generated `/etc/braid/config.json` shape (enabled path).
4. `just test-vm fan-control-disabled` — new test asserts `fan_control` key is absent when `braid.fanControl.enable = false`.
5. Manual on the NAS:
   - Rebuild NixOS with current `braid.fanControl` config (now using the `pwm.minStart` / `pwm.maxStop` shape).
   - `braid` → Data tab shows `Fans` section header `daemon: active` and live PWM/RPM matching `cat /sys/devices/platform/f71882fg.656/{fan2_input,...,pwm2}`.
   - Wait `FAN_PROBE_INTERVAL` (5s); fan values update without pressing `r` (verifies auto-poll).
   - Verify pool data does **not** auto-update (existing behavior preserved): the "Disks" `Temp` column session min/max watermarks should not advance the sample count between manual refreshes — these are reset by `R` and only updated by pool probes. (The unit tests in `app.rs` are the authoritative invariant; this is a sanity check.)
   - `sudo systemctl stop hddfancontrol-braid.service`; verify within one fan tick the section header switches to `daemon: inactive` (yellow) and sensor row dims.
   - `sudo systemctl start hddfancontrol-braid.service`; verify it returns to `daemon: active` (green).
   - `braid lock` to unmount pool; verify Fans section still shows PWM/RPM/Driving (drivetemp doesn't require LUKS unlock).
   - Set `braid.fanControl.enable = lib.mkForce false`, rebuild; verify Fans section disappears entirely.

## Critical files

- `modules/braid/fan-control.nix` (move `minStart`/`maxStop` into `pwm` group; update assertion + script substitutions)
- `tests/module/fan-control.nix` + `tests/module/fan-control-hotswap.nix` (move test fixture's `minStart`/`maxStop` into `pwm`)
- `manual/guides/fan-control.md` + `manual/guides/nixos-configuration.md` (update example option paths)
- `README.md` (mention fan telemetry + daemon health on the Dashboard feature; add the Fan Control guide to the guide list — per `AGENTS.md` "User Guide" instruction)
- `cli/src/config.rs` (extend struct + tests)
- `cli/src/cmd.rs` (add `CmdRequest::SystemctlIsActive` variant)
- `cli/src/tui/model.rs` (extend `Model`; add fan structs incl. `DaemonStatus`)
- `cli/src/tui/mod.rs` (1 callsite + demo callsite)
- `cli/src/tui/probe.rs` (add `probe_fan_for_tui` + helpers + tests, incl. `probe_daemon_status`)
- `cli/src/tui/effect.rs` (add `Effect::ProbeFan` + `Effect::ScheduleFanProbe` + `FAN_PROBE_INTERVAL`; pool effects unchanged)
- `cli/src/tui/event.rs` (add `Event::FanProbeFinished` + `Event::PollFanRefresh`; pool event unchanged)
- `cli/src/tui/app.rs` (add `Message::FanProbeFinished` + `Message::RefreshFan` handlers using flow `ScheduleFanProbe → PollFanRefresh → RefreshFan`; `model.fan_probe_inflight` flag with re-arm-only semantics on duplicate; pool refresh behavior unchanged)
- `cli/src/tui/view/mod.rs` (new render fn + `section_block_with_status` + layout branch + 7 snapshot tests)
- `modules/braid/cli.nix` (extend JSON generator)
- `tests/module/fan-control.py` (extend with config.json assertions)
- `tests/module/fan-control-disabled.nix` + `.py` (new — verify omission path)

## Reused existing patterns

- `section_block(title)` (`cli/src/tui/view/mod.rs:92-98`) — cyan-bordered section header.
- `pool_df_table` (`cli/src/tui/view/mod.rs:171-205`) — template for a single-table-in-a-section render.
- `Model::new_demo` + `sample_pool` (`cli/src/tui/view/mod.rs:1117+`) — fixture pattern for snapshot tests.
- `pkgs.formats.json` + camelCase Nix → snake_case serde — already established in `modules/braid/cli.nix:12`.
- Hwmon glob resolution logic — copied from `modules/braid/fan-control.nix:166-187`.
- ATA drive selector — mirrors `reference/hddfancontrol/src/cl.rs:117-135` (`ata-*` minus `*-partN` from `/dev/disk/by-id`).
- Drivetemp enumeration — mirrors `reference/hddfancontrol/src/probe/drivetemp.rs:20-46` with sysfs root injection for testability.
- `tempfile::TempDir` for sysfs/dev mocking — same approach as `cli/src/tui/probe.rs:280-283` already uses for `StatePaths`.
