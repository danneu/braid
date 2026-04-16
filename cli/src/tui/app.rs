use std::collections::VecDeque;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::tui::effect::Effect;
use crate::tui::model::{Model, PoolState, PoolStatus, TemperatureWatermark};
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
            if model.pool.is_inflight() {
                return vec![];
            }
            model.spinner_deadline = Some(Instant::now() + Duration::from_millis(500));
            if let Some(stale) = model.pool.current().cloned() {
                model.pool = PoolStatus::Refreshing(stale);
            } else {
                model.pool = PoolStatus::Loading;
            }
            vec![Effect::ProbePool {
                mount_point: model.mount_point.clone(),
                disk_by_id: model.disk_by_id.clone(),
                paths: model.paths.clone(),
            }]
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::tests::{sample_disk_names, sample_pool};

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
