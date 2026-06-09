# Plan: "online => awake" drive-wake posture (+ TUI online auto-refresh)

## Context

braid currently takes a blanket anti-wake stance: it assumes pool disks may be
asleep at any time and tip-toes to avoid waking them. The one concrete runtime
expression of this is the TUI's **manual-only** pool probe -- the heavy
`smartctl -H -A` per-disk + btrfs probe never auto-refreshes, locked in by the
test `cli/src/tui/app.rs#pool_probe_finished_returns_no_effects`, whose comment
cites ADR-015 and ADR-016 as the anti-wake justification.

This posture is being narrowed. The new model:

- **Online (mounted/unlocked) => disks are treated as awake.** braid probes and
  refreshes freely; no online-side standby detection (`smartctl -n standby`, no
  `Standby` health state).
- **Locked/offline => drive power state is not this decision's concern.** A
  *future, opt-in* `braid.autoSpinDown` (NOT built here) will be the only thing
  that ever parks a drive, and only in the locked state after a dwell time. That
  feature will own the "don't wake a locked, possibly-parked member" rule.

Today nothing parks drives (braid sets no `hdparm -S`), so the assumption cannot
be wrong in a way that matters; when it is wrong (operator-set standby timer),
the cost is a single wake-on-read -- identical to today.

Two things follow: (1) the TUI can auto-refresh the pool view while online, and
(2) several docs/comments that assert the blanket anti-wake stance must be
reconciled to the narrowed posture.

Confirmed during exploration -- the exact read-path boundary (an earlier draft's
"status reads no SMART" claim was wrong):
- **Automatic** reads: only the new TUI auto-loop, and its SMART read is
  **mount-gated** -- `cli/src/tui/probe.rs#probe_pool_for_tui` returns before its
  `SmartctlHealthJson` loop when `!domain.mounted`. Note the *other* pre-gate work
  is not all non-waking: `build_disk_luks_states` runs before that gate and calls
  `probe_disk_luks_metadata` -> `cryptsetup luksDump --dump-json-metadata <by-id>`,
  which reads the on-disk LUKS2 header and **can wake a parked drive** (only
  `cryptsetup status`, the mapper read, is non-waking). That pre-gate LUKS read fires
  at TUI startup and on manual `r`, including for locked members -- it is
  pre-existing and out of scope. The new auto-loop never triggers it on a locked
  member because the loop only re-arms while `PoolStatus::Mounted` (members unlocked
  and already spinning).
- **User-invoked** reads are explicit and may touch disks regardless of mount state:
  - `cli/src/status.rs#build_status` probes `smartctl` per disk but returns
    `not_mounted_status` first when `!pool.mounted` -- so `status` live SMART is
    **mounted-only**.
  - `cli/src/doctor.rs#check_smart_selftests` runs `SmartctlSelftestLogJson` even
    when unmounted, falling back to the member `by_id` path -- **not mount-gated**.
  - TUI Browse-tab SMART (`cli/src/tui/browse/state.rs`) likewise targets `by_id`
    and can probe locked members.
- "Online" for *this feature* means the **mounted live pool** (`PoolStatus::Mounted`
  in the TUI; `pool.mounted` in `status`/`doctor`). `braid-online.service` /
  `cli/src/online_state.rs#OnlineStateOps` is a correlated systemd lifecycle marker
  these read paths do **not** consult; it is the handle the *future* `autoSpinDown`
  will gate on.
- The auto-refresh loop runs for the lifetime of the interactive `braid tui`
  process -- there is **no** terminal-focus or foreground gate (`cli/src/tui/event.rs#InputHandler`
  forwards input/resize events but never gates the scheduler on focus), so it keeps
  ticking even in a detached or ignored terminal. What bounds it is not foregrounding
  but two other facts: it is **not** a systemd daemon or `status`/`doctor`-style
  monitor poller (it exists only while a user has `braid tui` open, and dies with the
  process), and it is mount-gated (re-arms only while `PoolStatus::Mounted`). So it
  does not reintroduce the "poller wakes a locked drive" concern.

## Posture (the invariant to encode)

> While the pool's filesystem is **mounted** (`PoolStatus::Mounted` in the TUI,
> `pool.mounted` in `status`/`doctor`), braid treats member disks as awake and
> reads/refreshes them freely. The anti-wake concern applies only to the **locked**
> state and is owned by the future opt-in `braid.autoSpinDown` feature (which gates
> on `braid-online.service`). braid adds no online-side standby detection. Explicit
> user-invoked diagnostics (`doctor` self-test, Browse-tab SMART) may still touch
> disks by `by_id` regardless of mount state.

## Part 1 -- TUI: auto-refresh the pool probe while online

Mirror the existing fan/UPS self-scheduling loop, but gate the reschedule on
online state. The fan loop is the template: `Effect::ScheduleFanProbe` ->
`spawn_worker` sleeps -> `Event::PollFanRefresh` -> `Message::RefreshFan`, guarded
by `fan_scheduler_pending` + an inflight check
(`cli/src/tui/app.rs#update` fan arms, `cli/src/tui/effect.rs#execute_effect`).

**Reuse, do not reinvent:** the pool already encodes inflight via
`cli/src/tui/model.rs#PoolStatus::is_inflight` (Loading|Refreshing) -- use it as
the inflight guard instead of adding a `pool_probe_inflight` bool. Only one new
state field is needed.

### Changes

1. **`cli/src/tui/model.rs`** -- add `pool_scheduler_pending: bool` to `Model`
   (mirror `fan_scheduler_pending`/`ups_scheduler_pending`). Initialize `false`
   in both `Model::new` and `Model::new_demo`. `Model::new` already seeds the
   first `Effect::ProbePool`, so the loop self-starts when that boot probe finds
   the pool mounted -- no init change beyond the field.

2. **`cli/src/tui/effect.rs`**
   - Add const `POOL_PROBE_INTERVAL: Duration = Duration::from_secs(10)`.
     **This 10s cadence is the one tunable to confirm in review** -- still slower
     than the 5s fan/UPS loop (the heavy smartctl+btrfs probe is far costlier than
     a fan/UPS read), but tight enough to feel like an interactive live dashboard
     for scrub/balance/error monitoring, and non-waking because online.
   - Add `Effect::SchedulePoolProbe { delay: Duration }`.
   - In `execute_effect`, handle it exactly like `Effect::ScheduleFanProbe`:
     `spawn_worker(cmd_tx, move || { thread::sleep(delay); Event::PollPoolRefresh },
     |_| Event::PollPoolRefresh)` (keep the same panic-hook-via-spawn_worker
     comment idiom used by the fan/UPS arms).

3. **`cli/src/tui/event.rs`** -- add `Event::PollPoolRefresh`; map it in
   `into_message`: `Event::PollPoolRefresh => Some(Message::PollPoolRefresh)`
   (mirror `PollFanRefresh => RefreshFan`).

4. **`cli/src/tui/app.rs`** -- add `Message::PollPoolRefresh`, and edit two
   handlers in `update`:

   - **`Message::PoolProbeFinished`** -- after computing `model.pool` and
     `probe_duration`, arm the loop only when the just-finished probe is online,
     and preserve the existing Browse-tab reload:
     ```rust
     let mut effects: Vec<Effect> = vec![];
     // Online => drives are awake; auto-refresh the view. Manual-only while
     // locked/errored (NotMounted/Error/ErrorStale): leave the loop torn down.
     // See ADR-031.
     if matches!(model.pool, PoolStatus::Mounted(_)) && !model.pool_scheduler_pending {
         model.pool_scheduler_pending = true;
         effects.push(Effect::SchedulePoolProbe { delay: POOL_PROBE_INTERVAL });
     }
     if model.tab == Tab::Browse {
         effects.extend(browse_load_if_active(model));
     }
     effects
     ```
     The `!pool_scheduler_pending` guard means a manual `r` probe that finishes
     while a sleeper is already pending does not double-arm (same guarantee the
     fan loop has).

   - **`Message::PollPoolRefresh`** (new tick handler) -- mirror `RefreshFan`,
     with "config absent" replaced by "not online":
     ```rust
     model.pool_scheduler_pending = false;
     let Some(paths) = model.paths.clone() else { return vec![]; }; // demo mode
     // Tear the loop down while locked/offline. It re-arms via PoolProbeFinished
     // the next time a refresh finds the pool Mounted.
     if !matches!(model.pool, PoolStatus::Mounted(_) | PoolStatus::Refreshing(_)) {
         return vec![];
     }
     if model.pool.is_inflight() {
         // A probe is already running (e.g. manual `r`); re-arm and wait.
         model.pool_scheduler_pending = true;
         return vec![Effect::SchedulePoolProbe { delay: POOL_PROBE_INTERVAL }];
     }
     model.spinner_deadline = Some(Instant::now() + Duration::from_millis(500));
     model.pool = match model.pool.current().cloned() {
         Some(stale) => PoolStatus::Refreshing(stale),
         None => PoolStatus::Loading,
     };
     vec![Effect::ProbePool {
         mount_point: model.mount_point.clone(),
         disks: model.disks.clone(),
         paths,
     }]
     ```

   Manual `Message::RefreshPool` is unchanged.

### Tests (`cli/src/tui/app.rs`, pure `update()` tests)

- **Replace** `pool_probe_finished_returns_no_effects` with:
  - `pool_probe_finished_arms_scheduler_when_online` -- `Ok(Mounted)` emits exactly
    one `Effect::SchedulePoolProbe` and sets `pool_scheduler_pending`.
  - `pool_probe_finished_no_reschedule_when_locked` -- `Ok(None)` (NotMounted)
    emits no `SchedulePoolProbe`.
  - `pool_probe_finished_no_reschedule_on_error` -- `Err(..)` emits no
    `SchedulePoolProbe`.
  - `pool_probe_finished_does_not_double_arm_scheduler_when_pending` -- with
    `pool_scheduler_pending = true`, `Ok(Mounted)` emits no `SchedulePoolProbe` and
    leaves the flag `true`. Guards the `!pool_scheduler_pending` check: the other
    finish tests only cover pending=false, so dropping the guard would leak a
    duplicate sleeper thread yet still pass them.
- **Add** `PollPoolRefresh` tick tests:
  - `poll_pool_refresh_starts_probe_when_online_and_idle` -- Mounted + not inflight
    -> emits `Effect::ProbePool`.
  - `poll_pool_refresh_tears_down_when_locked` -- NotMounted -> empty.
  - `poll_pool_refresh_rearms_when_inflight` -- Refreshing -> re-emits
    `SchedulePoolProbe`, no duplicate `ProbePool`.
- **Add** `pool_probe_finished_browse_tab_arms_scheduler_and_reloads` -- with
  `model.tab = Tab::Browse` and a mounted result, assert the effects contain BOTH
  `Effect::SchedulePoolProbe` AND the Browse reload `Effect::BrowseRunCommand`
  (shape per the existing `next_tab_into_browse_emits_effect`). Without this, the
  listed tests would still pass if the scheduler effect accidentally *replaced*
  `browse_load_if_active` instead of extending it.
- **Add** an `is_schedule_pool` effect-matcher helper next to the existing
  `is_schedule_fan`/probe matchers.
- **Update** the `Model::new` scheduler-pending test to assert
  `pool_scheduler_pending` starts `false`.
- **Update the WHY comments** of `fan_probe_finished_schedules_only_fan_refresh`
  and `ups_probe_finished_schedules_only_ups_refresh`: the reason a fan/UPS tick
  must not trigger a pool probe is now **cost + loop independence** (a heavy
  smartctl+btrfs probe at 5s cadence), not "waking drives."

## Part 2 -- Docs / ADR reconciliation

1. **New ADR `docs/design/decisions/031-drive-wake-posture.md`** (next free number
   -- 030 is already `smart-btrfs-error-reporting`; Status: `Active`; house
   front-matter `intent:` + `status:`). Cite
   `> Principle: [HDD defaults](../principles.md#11-hdd-defaults)`. Sections:
   - **Context** -- the prior blanket anti-wake stance and why it is narrowing.
   - **Decision** -- the Posture box above. State the exact read-path boundary so
     the ADR does not over-claim: the TUI auto-loop is the only *automatic* read and
     is mount-gated; `status` live SMART is mounted-only; `doctor` self-test and
     Browse-tab SMART are explicit user-invoked reads that may target `by_id` even
     when unmounted/locked; "online" here means the mounted live pool
     (`PoolStatus::Mounted`/`pool.mounted`), with `braid-online.service` a correlated
     lifecycle marker the future `autoSpinDown` gates on, not consulted by these read
     paths. Do **not** claim locked-state TUI probes are non-waking: the existing
     startup/manual probe reads on-disk LUKS metadata (`cryptsetup luksDump`) by
     `by_id` for locked members and can wake them -- pre-existing and out of scope.
     The change's only contribution to locked-state quiet is that the new auto-loop
     is mount-gated. The locked anti-wake rule is deferred to and owned by future
     `braid.autoSpinDown` (park only in the locked state).
   - **Alternatives considered** -- (a) online-side standby detection
     (`-n standby` + `Standby` health state): rejected, adds parser/state
     complexity for a state that does not occur while mounted, and the cost of being
     wrong is one wake-on-read; (b) status-quo blanket anti-wake: rejected, forces
     manual-only refresh + standby machinery for a locked-only concern.
   - **See** -- plain code spans (not links, `cli/` is outside the mdBook root):
     `cli/src/tui/app.rs#update`, `cli/src/tui/probe.rs#probe_pool_for_tui`,
     `cli/src/status.rs#build_status`, `cli/src/doctor.rs#check_smart_selftests`,
     `cli/src/online_state.rs#OnlineStateOps`; and real markdown links to
     `015-hdd-defaults.md`, `016-auto-suspend.md`, and
     `030-smart-btrfs-error-reporting.md`. Name `braid.autoSpinDown` in prose only
     -- do **not** link a nonexistent doc (mdbook-linkcheck2 would fail).
   - Add the page to **`docs/SUMMARY.md`** under the decisions list (mirror an
     existing ADR entry) so it builds and linkchecks.

2. **Revise `docs/design/decisions/015-hdd-defaults.md`** (Active; reframe, don't
   gut). The noatime rationale is actually moot under the new model -- while online
   we treat drives as awake, and while locked the FS is unmounted (no reads at
   all), so noatime is not spindown management. Reframe both noatime references
   (the Context bullet and the `## See` bullet) to the surviving justification:
   noatime avoids relatime's read-triggered metadata write-amplification on RAID1.
   Add a `## See` bullet to `031-drive-wake-posture.md`.

3. **Revise the `noatime` doc comment** on `base_mount_options` in
   `cli/src/cmd.rs` -- drop "preventing HDD spindown"; state the write-amplification
   reason and point to ADR-031 ("braid treats online drives as awake; not spindown
   management"). ASCII only.

4. **Clarify `docs/design/decisions/016-auto-suspend.md`** (Active; light touch --
   it describes a shipped feature, do not rewrite its narrative). Add one
   scope-clarifying note + a `## See` link to ADR-031: 016 governs whole-system
   suspend-to-RAM (S3); its "HDDs can't rely on per-drive spindown" line is context
   for choosing system suspend and does not preclude a future opt-in per-drive
   `autoSpinDown` in the locked state.

5. **Reconcile existing ADR `docs/design/decisions/030-smart-btrfs-error-reporting.md`**
   (Active; preserve its decision). It currently justifies plain `smartctl` (no
   `-n standby`) on the grounds that braid does "no per-drive spindown ... whole-
   system suspend-to-RAM." Keep the SMART/error-reporting decision and its
   conclusion intact -- `status` SMART is mounted-only, so it never reads a parked
   drive and still needs no `-n standby` guard -- but repoint that spindown
   *rationale* at ADR-031: per-drive spindown is no longer categorically absent, it
   is deferred to the future locked-only `autoSpinDown`, which never overlaps
   `status`'s mounted-only probe. Add a `## See` link to `031-drive-wake-posture.md`.

6. **Update `docs/commands/tui.md`** (the `#refreshing` section and the `r` key
   row). It currently says pool/disk/scrub/alert data refresh only on demand via
   `r`, with Fans/UPS auto every 5s and the footer `Reload: r` spinner reflecting
   *only* the pool refresh. Update it: while the pool is **mounted**, pool data also
   auto-refreshes (~10s) and immediately on `r`; while not mounted it stays
   manual-only (`r`). The footer spinner now also ticks on the automatic pool
   refresh, not just manual `r`. Keep README.md in sync only if it documents TUI
   refresh cadence (it does not today -- verify during impl).

7. **Leave `plans/review/2026-04-30-auto-spindown.md` untouched** (per decision).
   ADR-031 records the "park only while locked" constraint; that stale plan's
   online-timer approach is reconciled if/when autoSpinDown is actually built.

## Non-goals (record explicitly, in ADR-031)

- No `smartctl -n standby` and no `Standby` SmartHealth variant.
- No `braid.autoSpinDown` implementation, no `hdparm` integration, no `.nix`
  module changes (so: **no VM tests in scope**).
- No change to smartd config, to `status`'s mounted-only SMART probe, or to the
  explicit `doctor`/Browse SMART reads (which intentionally may touch disks by
  `by_id`). Only the new TUI auto-loop is added.
- No auto-refresh while locked. The loop only re-arms while `PoolStatus::Mounted`.
  This is genuinely safety-relevant, not just UX/cost: a locked auto-refresh would
  re-run `cryptsetup luksDump --dump-json-metadata <by-id>` for locked members every
  interval (an on-disk header read that can wake a parked drive once `autoSpinDown`
  exists). The pre-existing startup/manual locked LUKS-metadata reads are unchanged
  and out of scope.

## Verification

1. **`just test-rust`** -- primary gate. The new/updated `update()` handler tests
   in `cli/src/tui/app.rs` are pure and deterministic; they fully cover the
   online-arm / locked-teardown / inflight-rearm logic. Confirm TUI snapshot tests
   (if any run here) are unchanged -- rendering is untouched.
2. **`just docs-build`** (the repo recipe: `nix develop .#docs -c mdbook build docs`,
   mirroring the CI cross-link gate) -- validates ADR-031's cross-links to
   015/016/030 and the new `docs/SUMMARY.md` entry via mdbook-linkcheck2; a broken
   link fails.
3. **`scripts/docs/check-see-paths.py`** -- confirms the `## See` paths in ADR-031
   and the revised 015/030 resolve.
4. **`just check-output-ascii`** -- sanity check after editing the `cmd.rs` comment
   (comments are out of scope for the scan, but cheap to confirm nothing else
   regressed).
5. **Manual TUI smoke (optional, needs a pool)** -- run the braid TUI against a
   mounted pool: the Data tab should refresh on its own roughly every 10s (spinner
   re-ticks) with no `r` keypress; against a locked/not-mounted pool it should stay
   static until `r`. The unit tests are the authoritative coverage; this is a
   confidence check only.

Scope is Rust (TUI) + docs only; per AGENTS.md test-scope guidance this is a
localized change -- focused `just test-rust` + the doc build/check scripts, no VM
suite.
