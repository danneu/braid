use std::collections::VecDeque;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::tui::effect::{Effect, FAN_PROBE_INTERVAL};
use crate::tui::model::{FanSnapshot, Model, PoolState, PoolStatus, TemperatureWatermark};
use crate::tui::state::{CmdId, CmdStatus, CommandState, Stream, MAX_LINES};

pub enum Message {
    Quit,
    ToggleHelp,
    NextTab,
    PrevTab,
    RefreshPool,
    SelectNextDisk,
    SelectPrevDisk,
    OpenDiskDetail,
    CloseDiskDetail,
    ResetTemperatureStats,
    Tick,
    CommandStarted {
        id: CmdId,
        cmd: String,
    },
    CommandOutput {
        id: CmdId,
        stream: Stream,
        line: String,
    },
    CommandFinished {
        id: CmdId,
        status: ExitStatus,
    },
    PoolProbeFinished(Box<Result<Option<PoolState>, String>>, Duration),
    /// A fan probe finished. Install the snapshot and re-arm the loop.
    FanProbeFinished(FanSnapshot),
    /// Scheduler tick from `Effect::ScheduleFanProbe`. The handler reads
    /// fresh fan_control + inflight state off Model before emitting the
    /// next `Effect::ProbeFan`.
    RefreshFan,
}

fn fan_probe_effect(model: &Model) -> Option<Effect> {
    let fc = model.fan_control.as_ref()?;
    Some(Effect::ProbeFan {
        sysfs_root: std::path::PathBuf::from("/sys"),
        dev_root: std::path::PathBuf::from("/dev"),
        disk_by_id: model.disk_by_id.clone(),
        fan_control: fc.clone(),
    })
}

pub fn update(model: &mut Model, msg: Message) -> Vec<Effect> {
    match msg {
        Message::Quit => {
            model.running = false;
            vec![]
        }
        Message::ToggleHelp => {
            model.show_help = !model.show_help;
            vec![]
        }
        Message::NextTab => {
            model.tab = model.tab.next();
            vec![]
        }
        Message::PrevTab => {
            model.tab = model.tab.prev();
            vec![]
        }
        Message::RefreshPool => {
            let mut effects: Vec<Effect> = vec![];
            if !model.pool.is_inflight() {
                model.spinner_deadline = Some(Instant::now() + Duration::from_millis(500));
                if let Some(stale) = model.pool.current().cloned() {
                    model.pool = PoolStatus::Refreshing(stale);
                } else {
                    model.pool = PoolStatus::Loading;
                }
                effects.push(Effect::ProbePool {
                    mount_point: model.mount_point.clone(),
                    disk_by_id: model.disk_by_id.clone(),
                    paths: model.paths.clone(),
                });
            }
            // Manual `r` refreshes the fan too, but only if one isn't
            // already in flight — the auto-poll will catch up otherwise.
            if model.fan_control.is_some() && !model.fan_probe_inflight
                && let Some(fan_effect) = fan_probe_effect(model) {
                    model.fan_probe_inflight = true;
                    effects.push(fan_effect);
                }
            effects
        }
        Message::SelectNextDisk => {
            let len = model.disk_names.len();
            if len > 0 {
                model.selected_disk = (model.selected_disk + 1) % len;
            }
            vec![]
        }
        Message::SelectPrevDisk => {
            let len = model.disk_names.len();
            if len > 0 {
                model.selected_disk = (model.selected_disk + len - 1) % len;
            }
            vec![]
        }
        Message::OpenDiskDetail => {
            model.show_disk_detail = true;
            vec![]
        }
        Message::CloseDiskDetail => {
            model.show_disk_detail = false;
            vec![]
        }
        Message::ResetTemperatureStats => {
            model.session_temperature_stats.clear();
            vec![]
        }
        Message::Tick => vec![],
        Message::CommandStarted { id, cmd } => {
            model.commands.insert(
                id,
                CommandState {
                    cmd,
                    status: CmdStatus::Running,
                    output: VecDeque::new(),
                },
            );
            vec![]
        }
        Message::CommandOutput {
            id,
            stream: _,
            line,
        } => {
            if let Some(state) = model.commands.get_mut(&id) {
                state.output.push_back(line);
                if state.output.len() > MAX_LINES {
                    state.output.pop_front();
                }
            }
            vec![]
        }
        Message::CommandFinished { id, status } => {
            if let Some(state) = model.commands.get_mut(&id) {
                state.status = CmdStatus::Finished(status);
            }
            vec![]
        }
        Message::PoolProbeFinished(result, elapsed) => {
            let stale = model.pool.current().cloned();
            model.pool = match *result {
                Ok(Some(pool)) => {
                    // Fold this tick's temperature readings into the
                    // session watermark map *before* moving `pool` into
                    // `model.pool`. Doing it the other way would force a
                    // second borrow of the just-moved value.
                    for reading in pool.disk_temperature_readings.values() {
                        model
                            .session_temperature_stats
                            .entry(reading.id.clone())
                            .and_modify(|w| {
                                w.min_celsius = w.min_celsius.min(reading.celsius);
                                w.max_celsius = w.max_celsius.max(reading.celsius);
                                w.sample_count = w.sample_count.saturating_add(1);
                            })
                            .or_insert(TemperatureWatermark {
                                min_celsius: reading.celsius,
                                max_celsius: reading.celsius,
                                sample_count: 1,
                            });
                    }
                    PoolStatus::Mounted(pool)
                }
                Ok(None) => PoolStatus::NotMounted,
                Err(e) => match stale {
                    Some(s) => PoolStatus::ErrorStale(e, s),
                    None => PoolStatus::Error(e),
                },
            };
            model.probe_duration = Some(elapsed);
            // TODO: re-enable auto-polling
            // vec![Effect::ScheduleProbe {
            //     mount_point: model.mount_point.clone(),
            //     delay: PROBE_INTERVAL,
            // }]
            vec![]
        }
        Message::FanProbeFinished(snapshot) => {
            model.fan = Some(snapshot);
            model.fan_probe_inflight = false;
            vec![Effect::ScheduleFanProbe {
                delay: FAN_PROBE_INTERVAL,
            }]
        }
        Message::RefreshFan => {
            if model.fan_control.is_none() {
                return vec![];
            }
            if model.fan_probe_inflight {
                return vec![Effect::ScheduleFanProbe {
                    delay: FAN_PROBE_INTERVAL,
                }];
            }
            let effect = match fan_probe_effect(model) {
                Some(e) => e,
                None => return vec![],
            };
            model.fan_probe_inflight = true;
            vec![effect]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanControl as FanControlCfg, Pwm};
    use crate::tui::model::{DaemonStatus, FanSnapshot};
    use crate::tui::view::tests::{sample_disk_names, sample_pool};

    fn is_probe_fan(e: &Effect) -> bool {
        matches!(e, Effect::ProbeFan { .. })
    }
    fn is_probe_pool(e: &Effect) -> bool {
        matches!(e, Effect::ProbePool { .. })
    }
    fn is_schedule_fan(e: &Effect) -> bool {
        matches!(e, Effect::ScheduleFanProbe { .. })
    }
    fn is_schedule_pool(e: &Effect) -> bool {
        matches!(e, Effect::ScheduleProbe { .. })
    }

    fn sample_fan_control() -> FanControlCfg {
        FanControlCfg {
            pwm: Pwm {
                platform_device: "f71882fg.656".to_owned(),
                number: 2,
                min_start: 70,
                max_stop: 60,
            },
            min_temp: 30,
            max_temp: 40,
            min_fan_speed_percent: 20,
        }
    }

    fn sample_fan_snapshot() -> FanSnapshot {
        FanSnapshot {
            fan: None,
            driving: None,
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        }
    }

    /*
     * Intent: RefreshPool must set a spinner_deadline so the footer spinner
     * shows for at least 500ms.
     *
     * Why it exists: Without a minimum visible duration, fast probes give no
     * visual feedback that a reload happened.
     *
     * Scenario: User presses 'r' on a fast local probe that returns in <50ms.
     */
    #[test]
    fn refresh_pool_sets_spinner_deadline() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        let before = Instant::now();
        update(&mut model, Message::RefreshPool);
        let after = Instant::now();
        let deadline = model.spinner_deadline.expect("should be set");
        assert!(deadline >= before + Duration::from_millis(500));
        assert!(deadline <= after + Duration::from_millis(500));
    }

    /*
     * Intent: PoolProbeFinished must NOT clear spinner_deadline, so the
     * spinner continues until the deadline expires naturally.
     *
     * Why it exists: Clearing it would defeat the minimum-duration guarantee.
     *
     * Scenario: Probe finishes in 50ms but spinner should stay for 500ms.
     */
    #[test]
    fn probe_finished_preserves_spinner_deadline() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        model.spinner_deadline = Some(Instant::now() + Duration::from_secs(10));
        let pool = sample_pool();
        update(
            &mut model,
            Message::PoolProbeFinished(Box::new(Ok(Some(pool))), Duration::from_millis(50)),
        );
        assert!(model.spinner_deadline.is_some());
    }

    // Helpers for the temperature-watermark tests below.
    use crate::tui::model::{TemperatureDiskId, TemperatureReading};
    use crate::types::LuksUuid;

    fn pool_with_temperature(name: &str, uuid_hex: &str, celsius: i16) -> PoolState {
        let mut pool = sample_pool();
        pool.disk_temperature_readings.insert(
            name.to_owned(),
            TemperatureReading {
                id: TemperatureDiskId::LuksUuid(LuksUuid(uuid_hex.to_owned())),
                celsius,
            },
        );
        pool
    }

    // Intent: a PoolProbeFinished that carries the first temperature reading
    //         for a disk must seed `session_temperature_stats` with
    //         min==max==current and sample_count=1.
    // Why: the render rule hides min/max until sample_count>=2; seeding
    //      any other way would either show a misleading range immediately
    //      or never reach the >=2 threshold.
    // Scenario: fresh Model, probe returns pool with toshiba at 38 C.
    #[test]
    fn probe_finished_seeds_temperature_watermark() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let pool = pool_with_temperature("toshiba", "11111111-1111-1111-1111-111111111111", 38);
        update(
            &mut model,
            Message::PoolProbeFinished(Box::new(Ok(Some(pool))), Duration::from_millis(10)),
        );
        let id = TemperatureDiskId::LuksUuid(LuksUuid(
            "11111111-1111-1111-1111-111111111111".to_owned(),
        ));
        let w = model
            .session_temperature_stats
            .get(&id)
            .expect("watermark seeded");
        assert_eq!(w.min_celsius, 38);
        assert_eq!(w.max_celsius, 38);
        assert_eq!(w.sample_count, 1);
    }

    // Intent: a second PoolProbeFinished with a higher celsius must widen
    //         max_celsius, leave min_celsius alone, and bump sample_count.
    // Why: this is the core of the "how hot did it get during my copy"
    //      observation -- the watermark only moves outward.
    // Scenario: first probe 38 C, second probe 44 C.
    #[test]
    fn probe_finished_widens_max_on_higher_sample() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let uuid = "11111111-1111-1111-1111-111111111111";
        let id = TemperatureDiskId::LuksUuid(LuksUuid(uuid.to_owned()));
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 38)))),
                Duration::from_millis(10),
            ),
        );
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 44)))),
                Duration::from_millis(10),
            ),
        );
        let w = model.session_temperature_stats.get(&id).unwrap();
        assert_eq!(w.min_celsius, 38);
        assert_eq!(w.max_celsius, 44);
        assert_eq!(w.sample_count, 2);
    }

    // Intent: a lower celsius must widen min_celsius, leave max alone.
    // Why: symmetric counterpart to the max-widen case.
    // Scenario: first probe 38 C, second probe 30 C.
    #[test]
    fn probe_finished_widens_min_on_lower_sample() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let uuid = "11111111-1111-1111-1111-111111111111";
        let id = TemperatureDiskId::LuksUuid(LuksUuid(uuid.to_owned()));
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 38)))),
                Duration::from_millis(10),
            ),
        );
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 30)))),
                Duration::from_millis(10),
            ),
        );
        let w = model.session_temperature_stats.get(&id).unwrap();
        assert_eq!(w.min_celsius, 30);
        assert_eq!(w.max_celsius, 38);
        assert_eq!(w.sample_count, 2);
    }

    // Intent: a probe that returns no reading for a disk must leave that
    //         disk's existing watermark untouched.
    // Why: the plan explicitly forbids a stale-current effect -- if a
    //      transient smartctl failure produced a silent reset of min/max,
    //      a user staring at the watermark during a fan test would see
    //      the range collapse unpredictably.
    // Scenario: seed toshiba watermark, then a probe arrives where toshiba's
    //           entry is missing from disk_temperature_readings entirely.
    #[test]
    fn probe_finished_missing_reading_preserves_watermark() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let uuid = "11111111-1111-1111-1111-111111111111";
        let id = TemperatureDiskId::LuksUuid(LuksUuid(uuid.to_owned()));
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 38)))),
                Duration::from_millis(10),
            ),
        );
        // Follow-up probe with a pool that has no temperature readings at
        // all -- simulates toshiba's smartctl briefly failing.
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(sample_pool()))),
                Duration::from_millis(10),
            ),
        );
        let w = model.session_temperature_stats.get(&id).unwrap();
        assert_eq!(w.min_celsius, 38);
        assert_eq!(w.max_celsius, 38);
        assert_eq!(w.sample_count, 1);
    }

    // Intent: FanProbeFinished must install the snapshot, clear the
    //         in-flight flag, and emit exactly one ScheduleFanProbe --
    //         no pool-probe side-effects.
    // Why: the two loops are independent. If FanProbeFinished accidentally
    //      pushed a pool effect, every fan tick would also run a heavy
    //      smartctl probe -- defeating the whole "cheap fan cadence"
    //      design decision (revision 7 in the plan).
    // Scenario: an in-flight fan probe returns; model is refreshed and
    //         the next scheduler tick is armed.
    #[test]
    fn fan_probe_finished_schedules_only_fan_refresh() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan_probe_inflight = true;
        let effects = update(&mut model, Message::FanProbeFinished(sample_fan_snapshot()));
        assert!(model.fan.is_some());
        assert!(!model.fan_probe_inflight);
        assert_eq!(effects.len(), 1);
        assert!(is_schedule_fan(&effects[0]));
        assert!(!effects.iter().any(is_probe_pool));
        assert!(!effects.iter().any(is_schedule_pool));
    }

    // Intent: PoolProbeFinished must NOT auto-reschedule pool probes.
    // Why: the pool probe is heavy (smartctl -H -A per disk, btrfs
    //      commands). Auto-rescheduling would wake sleeping drives and
    //      interfere with HDD spindown. This test locks in the
    //      manual-only contract so a future contributor doesn't
    //      uncomment the TODO without understanding the trade-off.
    // Scenario: any pool probe completion.
    #[test]
    fn pool_probe_finished_returns_no_effects() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let effects = update(
            &mut model,
            Message::PoolProbeFinished(Box::new(Ok(Some(sample_pool()))), Duration::from_millis(10)),
        );
        assert!(effects.is_empty(), "got {} effects", effects.len());
    }

    // Intent: RefreshFan with a fan probe already in flight re-arms the
    //         scheduler but does NOT spawn a duplicate ProbeFan.
    // Why: a duplicate probe would race with the inflight one --
    //      FanProbeFinished clears inflight, so a second finish could
    //      land while the loop thinks nothing is running. Silently
    //      dropping the loop (returning []) is also wrong because the
    //      single scheduler thread would die. Re-arming preserves the
    //      loop without adding work.
    // Scenario: user presses `r` during a slow fan probe, then the
    //         auto-tick fires before the probe returns.
    #[test]
    fn refresh_fan_skips_when_inflight() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        model.fan_control = Some(sample_fan_control());
        model.fan_probe_inflight = true;
        let effects = update(&mut model, Message::RefreshFan);
        assert_eq!(effects.len(), 1);
        assert!(is_schedule_fan(&effects[0]));
        assert!(!effects.iter().any(is_probe_fan));
    }

    // Intent: RefreshFan with no fan_control tears the loop down
    //         cleanly (returns no effects).
    // Why: if the daemon is disabled mid-session (unlikely but possible
    //      via a reload path), the loop must stop on its own rather
    //      than spin forever firing empty probes.
    // Scenario: fan_control is None when a scheduler tick lands.
    #[test]
    fn refresh_fan_skips_when_disabled() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        model.fan_control = None;
        let effects = update(&mut model, Message::RefreshFan);
        assert!(effects.is_empty());
    }

    // Intent: RefreshFan with fan_control set and no probe in flight
    //         emits exactly one Effect::ProbeFan AND flips
    //         model.fan_probe_inflight to true.
    // Why: the inflight flag is the guard that prevents duplicate
    //      probes across the manual-refresh and auto-poll paths.
    //      Setting it here, at the point of decision, is the single
    //      source of truth -- downstream FanProbeFinished is what
    //      clears it.
    // Scenario: auto-poll tick on an idle model with fan enabled.
    #[test]
    fn refresh_fan_emits_probe_when_idle() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        model.fan_control = Some(sample_fan_control());
        model.fan_probe_inflight = false;
        let effects = update(&mut model, Message::RefreshFan);
        assert!(effects.iter().any(is_probe_fan));
        assert!(model.fan_probe_inflight);
        // Match on ProbeFan and verify the fan_control carried through.
        let probe = effects
            .iter()
            .find(|e| matches!(e, Effect::ProbeFan { .. }))
            .unwrap();
        if let Effect::ProbeFan { fan_control, .. } = probe {
            assert_eq!(fan_control.pwm.number, 2);
            assert_eq!(fan_control.pwm.platform_device, "f71882fg.656");
        }
    }

    // Intent: manual `r` with fan enabled and idle fires BOTH pool and
    //         fan probes.
    // Why: the user's mental model of `r` is "refresh everything". The
    //      plan pins manual refresh as a both-probe trigger so the user
    //      doesn't need to learn separate keystrokes for each subsystem.
    // Scenario: user presses `r` on a fan-enabled system.
    #[test]
    fn refresh_pool_with_fan_idle_emits_both() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan_probe_inflight = false;
        let effects = update(&mut model, Message::RefreshPool);
        assert!(effects.iter().any(is_probe_pool), "missing ProbePool");
        assert!(effects.iter().any(is_probe_fan), "missing ProbeFan");
        assert!(model.fan_probe_inflight);
    }

    // Intent: manual `r` while a fan probe is already in flight emits
    //         only the pool effect, not a duplicate ProbeFan.
    // Why: mirror of the RefreshFan guard -- the two entry points must
    //      use the same inflight logic or duplicate probes leak in.
    // Scenario: user presses `r` repeatedly during a slow fan probe.
    #[test]
    fn refresh_pool_with_fan_inflight_emits_only_pool() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan_probe_inflight = true;
        let effects = update(&mut model, Message::RefreshPool);
        assert!(effects.iter().any(is_probe_pool));
        assert!(!effects.iter().any(is_probe_fan));
    }

    // Intent: manual `r` on a fan-disabled system emits only the pool
    //         effect.
    // Why: no fan_control = no fan work. This is the common case for
    //      users who haven't enabled fanControl; breaking it would
    //      cause spurious probes for every NAS in the field.
    // Scenario: standard `r` press on a default-config system.
    #[test]
    fn refresh_pool_with_fan_disabled_emits_only_pool() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = None;
        let effects = update(&mut model, Message::RefreshPool);
        assert!(effects.iter().any(is_probe_pool));
        assert!(!effects.iter().any(is_probe_fan));
        assert!(!effects.iter().any(is_schedule_fan));
    }

    // Intent: Message::ResetTemperatureStats must empty the watermark map.
    // Why: this is what Shift+R does; users rely on it to start a fresh
    //      observation window during a new test.
    // Scenario: populated stats map, dispatch ResetTemperatureStats,
    //           verify empty.
    #[test]
    fn reset_temperature_stats_clears_map() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let uuid = "11111111-1111-1111-1111-111111111111";
        update(
            &mut model,
            Message::PoolProbeFinished(
                Box::new(Ok(Some(pool_with_temperature("toshiba", uuid, 38)))),
                Duration::from_millis(10),
            ),
        );
        assert!(!model.session_temperature_stats.is_empty());
        update(&mut model, Message::ResetTemperatureStats);
        assert!(model.session_temperature_stats.is_empty());
    }
}
