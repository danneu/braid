# Plan: delete dead pool auto-poll plumbing in the TUI

## Context

The TUI was once driven by an auto-poll loop: `Message::PoolProbeFinished` would
emit an `Effect::ScheduleProbe`, which spawned a sleeper that sent
`Event::PollRefresh`, which mapped back to `Message::RefreshPool`. That loop was
disabled in commit `63380ef` ("tui: progress", 2026-02-27) by commenting out
the only emitter and replacing it with `vec![]`. The plumbing was left in place
behind a `// TODO: re-enable auto-polling` comment.

Auto-polling cannot come back the way the TODO suggests. Each pool probe runs
`smartctl -H -A` per disk plus several btrfs subcommands, all of which wake
sleeping HDDs. That directly contradicts the spindown story in
`docs/decisions/015-hdd-defaults.md` and the broader "do not wake the system"
posture in `docs/decisions/016-auto-suspend.md`. The contract is already pinned
by the regression test `pool_probe_finished_returns_no_effects` at
`cli/src/tui/app.rs:923-934`.

What remains is three coupled layers of unreachable code:

- `Effect::ScheduleProbe` -- variant + executor arm + commented-out producer +
  one test helper. Zero live producers.
- `Event::PollRefresh` -- variant + `into_message` mapping. The only producer is
  inside the dead `execute_effect` arm above.
- `PROBE_INTERVAL` -- the constant the dead arm uses. The two live cadence
  constants (`FAN_PROBE_INTERVAL`, `UPS_PROBE_INTERVAL`) are distinct and stay.

This is reader-hostile: a future contributor either thinks pool auto-polling
exists and "re-enables" it (re-introducing the HDD spindown regression), or
copies the wiring for a new subsystem and misses that the live fan/UPS loops
depend on `RefreshPool`'s `is_inflight` guard plus per-loop
`*_scheduler_pending` flags. The TODO's framing makes the code look authoritative
when it is actually a footgun.

Intended outcome: the dead code is gone, the regression test gains a doc anchor
so the manual-only contract is discoverable, and no behavior changes.

## Changes

Three files, all under `cli/src/tui/`. Exhaustive list -- no pattern repeats.

### `cli/src/tui/effect.rs`

- Delete `PROBE_INTERVAL` (line 14).
- Delete `Effect::ScheduleProbe { mount_point, delay }` variant (lines 29-32).
- Delete its `execute_effect` arm (lines 87-93), which is the only producer of
  `Event::PollRefresh`.

Keep `FAN_PROBE_INTERVAL`, `UPS_PROBE_INTERVAL`, and all other variants.

### `cli/src/tui/event.rs`

- Delete `Event::PollRefresh { mount_point: MountPoint }` variant (lines 24-26).
- Delete its `into_message` arm: `Event::PollRefresh { .. } => Some(Message::RefreshPool)` (line 49).
- If `MountPoint` is no longer referenced in the file after the deletion, drop
  the `use crate::types::MountPoint;` import (line 14). Check after editing --
  do not pre-judge.

### `cli/src/tui/app.rs`

- Delete the commented-out TODO block in `Message::PoolProbeFinished` (lines
  265-269) -- the `// TODO: re-enable auto-polling` line and the four commented
  `Effect::ScheduleProbe { ... }` lines. The `vec![]` return immediately below
  stays.
- Delete the `is_schedule_pool` test helper (lines 368-370).
- Delete its two callsites:
  - `cli/src/tui/app.rs:841` inside `fan_probe_finished_schedules_only_fan_refresh`
  - `cli/src/tui/app.rs:1099` inside `ups_probe_finished_schedules_only_ups_refresh`

  Both are `assert!(!effects.iter().any(is_schedule_pool));`. They become
  vacuous once the variant is gone; the surrounding assertions
  (`assert_eq!(effects.len(), 1)` plus `is_schedule_fan`/`is_schedule_ups` on
  index 0) already pin the "exactly one schedule, of the right kind" invariant.
- Update the comment on `pool_probe_finished_returns_no_effects` (lines 916-922)
  to cite the design docs explicitly. Replace the TODO-anchored sentence
  ("a future contributor doesn't uncomment the TODO without understanding the
  trade-off") with a doc reference, since the TODO is being deleted. Proposed
  rewording:

  ```
  // Intent: PoolProbeFinished must NOT auto-reschedule pool probes.
  // Why: the pool probe is heavy (smartctl -H -A per disk, btrfs
  //      commands). Auto-rescheduling would wake sleeping drives and
  //      contradict the HDD spindown posture from
  //      docs/decisions/015-hdd-defaults.md and the anti-wake stance in
  //      docs/decisions/016-auto-suspend.md. This test locks in the
  //      manual-only contract; reintroducing a scheduler here needs to
  //      revisit those decision docs first.
  // Scenario: any pool probe completion.
  ```

  The test body is unchanged.

## Files modified

- `cli/src/tui/effect.rs`
- `cli/src/tui/event.rs`
- `cli/src/tui/app.rs`

No changes to docs, plans, scripts, or NixOS modules. Live-code grep,
scoped to Rust sources only --

```
rg -n "ScheduleProbe|PollRefresh|PROBE_INTERVAL|is_schedule_pool" cli/src
```

-- confirmed zero hits outside these three files. (A repo-wide grep also
matches historical prose under `plans/`, e.g. `plans/impl/2026-04-20-tui-fans-section.md`
which discusses the old scheduler pattern; those are documentation of
prior state and are intentionally not edited.)

## Verification

1. `just test-rust` -- the affected test module is `cli/src/tui/app.rs` and the
   smaller `cli/src/tui/event.rs` test module. Both must pass. In particular:
   - `pool_probe_finished_returns_no_effects` still passes (assertion is
     "empty effects vec"; doesn't depend on the deleted variant existing).
   - `fan_probe_finished_schedules_only_fan_refresh` and
     `ups_probe_finished_schedules_only_ups_refresh` still pass with their
     remaining assertions (the `len == 1` + correct-kind checks already pin
     the invariant the deleted assertions were redundantly enforcing).
   - The fan/UPS scheduler loop tests
     (`refresh_fan_idle_tick_rearms_once_after_probe_finished` etc.) continue
     to pass -- they never touched pool-schedule effects.

   Manual-refresh behavior (the live `Message::RefreshPool` path that runs
   when `model.paths` is `Some`) is already covered by the existing non-demo
   unit tests in `cli/src/tui/app.rs`:
   `refresh_pool_sets_spinner_deadline`,
   `refresh_then_probe_err_yields_error_stale_preserving_pool`,
   `refresh_pool_with_fan_idle_emits_both`,
   `refresh_pool_with_fan_inflight_emits_only_pool`,
   `refresh_pool_with_fan_disabled_emits_only_pool`,
   `refresh_pool_fan_piggyback_does_not_double_arm_scheduler`,
   `refresh_pool_with_ups_idle_emits_both`,
   `refresh_pool_with_ups_inflight_emits_only_pool`, and
   `refresh_pool_ups_piggyback_does_not_double_arm_scheduler`. These all
   construct a model with a real `StatePaths` via `tempfile::tempdir`, so
   they exercise the same branch that production `r` takes. No new tests
   are needed.
2. `cargo check -p braid-cli` -- catches any missed import (e.g. the
   `MountPoint` use in `event.rs` if it ends up unused).
3. `cargo clippy -p braid-cli --all-targets` -- catches dead-code or
   unused-import warnings that might surface after the deletions.
4. Manual smoke: `cargo run -p braid-cli -- tui --demo` to confirm the TUI
   still boots, the Data tab still renders, and the static demo pool is
   visible. This step is intentionally narrow: in demo mode
   `Model::new_demo` sets `paths: None`, and `Message::RefreshPool`
   short-circuits to `vec![]` at `cli/src/tui/app.rs:94-97`, so pressing
   `r` is a no-op here -- it cannot verify the refresh path. The
   refresh-path coverage lives in the unit tests listed above.

## Out of scope

- Re-enabling auto-polling. If that work happens, it needs its own decision
  doc reconciling with 015/016, not a quiet uncomment of a TODO.
- Touching `FAN_PROBE_INTERVAL` / `UPS_PROBE_INTERVAL` or their scheduler
  loops -- those are live and correct.
- Renaming the remaining `mount_point` plumbing in `Effect::ProbePool`. That
  field is still used.
