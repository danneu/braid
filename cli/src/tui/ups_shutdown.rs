//! What the TUI says about early shutdown on battery. One pure classifier
//! turns the policy and the latest UPS probe into a state; the UPS section's
//! Shutdown column and the on-battery banner both render from that state, so
//! they cannot disagree (plan/2026-10-23-0548/D4).

use std::time::Duration;

use ratatui::style::Color;

use crate::config::UpsShutdownPolicy;
use crate::parse::types::UpsStatusFlag;
use crate::tui::model::{GuardClock, UpsSnapshot};
use crate::ups_guard::TRIGGER_LAG_BOUND;
use crate::util::format_duration_secs;

/// Below this much time left on the timer, the countdown turns red.
const URGENT: Duration = Duration::from_secs(60);

/// The early-shutdown state the operator sees, highest priority first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownDisplay {
    /// `FSD` is in `ups.status`: a forced shutdown is under way, whoever
    /// raised it.
    ShuttingDown,
    /// Both triggers are null; only firmware low battery shuts down.
    Off { on_battery: bool },
    /// A trigger is set but the guard is not running, so it cannot fire
    /// (plan/2026-10-23-0548/I2).
    GuardDown { on_battery: bool },
    /// The timer passed its deadline by more than the guard's trigger lag and
    /// no FSD is visible: the guard failed to shut down.
    Overdue { on_battery: Duration },
    /// The guard is timing an outage.
    Counting {
        on_battery: Duration,
        /// Time left on the timer; `None` when no timer is set.
        remaining: Option<Duration>,
        floor_pct: Option<u8>,
        charge_pct: Option<u8>,
    },
    /// `ups.status` says OB but the guard has not written its clock yet.
    Starting,
    /// The guard's clock could not be read.
    ClockError,
    /// The policy is in force and nothing is happening.
    Armed {
        after: Option<Duration>,
        floor_pct: Option<u8>,
    },
}

/// Classify one probe against the policy. `None` when early shutdown is on
/// but the probe carries no guard reading (the panic fallback snapshot), so
/// the view shows "--" rather than guess.
///
/// The countdown comes only from the guard's clock file, never from this
/// probe's `ups.status` (plan/2026-10-23-0548/D3, I1).
pub fn classify(policy: &UpsShutdownPolicy, snapshot: &UpsSnapshot) -> Option<ShutdownDisplay> {
    let on_battery = snapshot.flags.contains(&UpsStatusFlag::Ob);
    if snapshot.flags.contains(&UpsStatusFlag::Fsd) {
        return Some(ShutdownDisplay::ShuttingDown);
    }
    if !policy.is_enabled() {
        return Some(ShutdownDisplay::Off { on_battery });
    }
    let guard = snapshot.guard.as_ref()?;
    if !guard.running {
        return Some(ShutdownDisplay::GuardDown { on_battery });
    }
    let after = policy.after_secs.map(|s| Duration::from_secs(s.get()));
    let floor_pct = policy.below_charge_pct.map(|f| f.get());
    Some(match guard.clock {
        GuardClock::Running {
            on_battery: elapsed,
        } => {
            if after.is_some_and(|limit| elapsed > limit.saturating_add(TRIGGER_LAG_BOUND)) {
                ShutdownDisplay::Overdue {
                    on_battery: elapsed,
                }
            } else {
                ShutdownDisplay::Counting {
                    on_battery: elapsed,
                    remaining: after.map(|limit| limit.saturating_sub(elapsed)),
                    floor_pct,
                    charge_pct: snapshot.battery_charge_pct,
                }
            }
        }
        GuardClock::Idle if on_battery => ShutdownDisplay::Starting,
        GuardClock::Error => ShutdownDisplay::ClockError,
        GuardClock::Idle => ShutdownDisplay::Armed { after, floor_pct },
    })
}

fn secs(d: Duration) -> String {
    format_duration_secs(d.as_secs())
}

/// Red under a minute left on the timer, else yellow.
fn countdown_color(remaining: Option<Duration>) -> Color {
    match remaining {
        Some(left) if left < URGENT => Color::Red,
        _ => Color::Yellow,
    }
}

/// Column text for the trigger parts that are set: "in 3m 12s or <50%",
/// "in 3m 12s", or "below 50%". `lead` is "in" while counting, "after"
/// while armed.
fn trigger_parts(lead: &str, timer: Option<Duration>, floor_pct: Option<u8>) -> String {
    match (timer, floor_pct) {
        (Some(t), Some(f)) => format!("{lead} {} or <{f}%", secs(t)),
        (Some(t), None) => format!("{lead} {}", secs(t)),
        (None, Some(f)) => format!("below {f}%"),
        (None, None) => "--".into(),
    }
}

impl ShutdownDisplay {
    /// The UPS section's Shutdown cell.
    pub fn column(&self) -> (String, Color) {
        match self {
            Self::ShuttingDown => ("shutting down".into(), Color::Red),
            Self::Off { .. } => ("off".into(), Color::DarkGray),
            Self::GuardDown { .. } => ("guard down".into(), Color::Red),
            Self::Overdue { .. } => ("overdue".into(), Color::Red),
            Self::Counting {
                remaining,
                floor_pct,
                ..
            } => (
                trigger_parts("in", *remaining, *floor_pct),
                countdown_color(*remaining),
            ),
            Self::Starting => ("timing...".into(), Color::Yellow),
            Self::ClockError => ("clock error".into(), Color::Red),
            Self::Armed { after, floor_pct } => {
                (trigger_parts("after", *after, *floor_pct), Color::DarkGray)
            }
        }
    }

    /// The banner shown on every tab, or `None` when nothing needs the
    /// operator's attention (plan/2026-10-23-0548/D5).
    pub fn banner(&self) -> Option<(String, Color)> {
        match self {
            Self::ShuttingDown => Some((
                "UPS forced shutdown -- braid is shutting down".into(),
                Color::Red,
            )),
            Self::Off { on_battery: true } => Some((
                "on battery -- early shutdown is off; braid shuts down at UPS low battery".into(),
                Color::Yellow,
            )),
            Self::GuardDown { on_battery: true } => Some((
                "on battery and braid-ups-guard is down -- only UPS low battery will shut down"
                    .into(),
                Color::Red,
            )),
            Self::Overdue { on_battery } => Some((
                format!(
                    "on battery {} -- shutdown overdue; check journalctl -u braid-ups-guard",
                    secs(*on_battery)
                ),
                Color::Red,
            )),
            Self::Counting {
                on_battery,
                remaining,
                floor_pct,
                charge_pct,
            } => {
                let charge = match charge_pct {
                    Some(pct) => format!("now {pct}%"),
                    None => "charge unknown".into(),
                };
                let when = match (remaining, floor_pct) {
                    (Some(left), Some(f)) => {
                        format!("in {}, or below {f}% charge ({charge})", secs(*left))
                    }
                    (Some(left), None) => format!("in {}", secs(*left)),
                    (None, Some(f)) => format!("below {f}% charge ({charge})"),
                    (None, None) => "at UPS low battery".into(),
                };
                Some((
                    format!(
                        "on battery {} -- braid shuts down {when}",
                        secs(*on_battery)
                    ),
                    countdown_color(*remaining),
                ))
            }
            Self::Starting => Some((
                "on battery -- braid-ups-guard is starting its shutdown clock".into(),
                Color::Yellow,
            )),
            Self::Off { on_battery: false }
            | Self::GuardDown { on_battery: false }
            | Self::ClockError
            | Self::Armed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChargeFloorPct, UpsShutdownPolicy};
    use crate::tui::model::{DaemonStatus, GuardSnapshot};
    use std::num::NonZeroU64;

    fn policy(after: Option<u64>, floor: Option<u8>) -> UpsShutdownPolicy {
        UpsShutdownPolicy {
            after_secs: after.and_then(NonZeroU64::new),
            below_charge_pct: floor.map(|f| ChargeFloorPct::try_from(f).unwrap()),
        }
    }

    fn both() -> UpsShutdownPolicy {
        policy(Some(300), Some(50))
    }

    fn snapshot(flags: &[UpsStatusFlag], guard: Option<GuardSnapshot>) -> UpsSnapshot {
        UpsSnapshot {
            flags: flags.to_vec(),
            battery_charge_pct: Some(78),
            runtime_secs: None,
            load_pct: None,
            watts_estimated: None,
            raw_text: String::new(),
            daemon: DaemonStatus::Active,
            guard,
        }
    }

    fn guard(running: bool, clock: GuardClock) -> Option<GuardSnapshot> {
        Some(GuardSnapshot { running, clock })
    }

    fn timing(secs: u64) -> GuardClock {
        GuardClock::Running {
            on_battery: Duration::from_secs(secs),
        }
    }

    const OB: &[UpsStatusFlag] = &[UpsStatusFlag::Ob];
    const OL: &[UpsStatusFlag] = &[UpsStatusFlag::Ol];

    fn text(d: &ShutdownDisplay) -> (String, Option<String>) {
        (d.column().0, d.banner().map(|b| b.0))
    }

    // Intent: every row of the plan/2026-10-23-0548/D4 table classifies and
    //   renders as specified, in both the column and the banner.
    // Why it exists: the column and banner are the operator's only view of the
    //   guard; a wrong row tells them the host is protected when it is not.
    // Scenario: each guard and UPS state in turn, with a 5 min timer and a
    //   50% floor.
    #[test]
    fn classify_covers_every_d4_row() {
        let cases: Vec<(UpsShutdownPolicy, UpsSnapshot, &str, Option<&str>)> = vec![
            (
                both(),
                snapshot(
                    &[UpsStatusFlag::Ob, UpsStatusFlag::Fsd],
                    guard(true, timing(310)),
                ),
                "shutting down",
                Some("UPS forced shutdown -- braid is shutting down"),
            ),
            (policy(None, None), snapshot(OL, None), "off", None),
            (
                policy(None, None),
                snapshot(OB, None),
                "off",
                Some("on battery -- early shutdown is off; braid shuts down at UPS low battery"),
            ),
            (
                both(),
                snapshot(OL, guard(false, GuardClock::Idle)),
                "guard down",
                None,
            ),
            (
                both(),
                snapshot(OB, guard(false, GuardClock::Idle)),
                "guard down",
                Some(
                    "on battery and braid-ups-guard is down -- only UPS low battery will shut down",
                ),
            ),
            (
                both(),
                snapshot(OB, guard(true, timing(320))),
                "overdue",
                Some("on battery 5m 20s -- shutdown overdue; check journalctl -u braid-ups-guard"),
            ),
            (
                both(),
                snapshot(OB, guard(true, timing(108))),
                "in 3m 12s or <50%",
                Some(
                    "on battery 1m 48s -- braid shuts down in 3m 12s, or below 50% charge (now 78%)",
                ),
            ),
            (
                both(),
                snapshot(OB, guard(true, GuardClock::Idle)),
                "timing...",
                Some("on battery -- braid-ups-guard is starting its shutdown clock"),
            ),
            (
                both(),
                snapshot(OB, guard(true, GuardClock::Error)),
                "clock error",
                None,
            ),
            (
                both(),
                snapshot(OL, guard(true, GuardClock::Idle)),
                "after 5m 0s or <50%",
                None,
            ),
        ];
        for (policy, snap, column, banner) in cases {
            let display = classify(&policy, &snap).unwrap();
            assert_eq!(
                text(&display),
                (column.to_owned(), banner.map(str::to_owned)),
                "{display:?}"
            );
        }
    }

    // Intent: when two states hold, the higher row wins: FSD beats off and
    //   guard down, and guard down beats a running countdown.
    // Why it exists: plan/2026-10-23-0548/D4 priority -- a forced shutdown
    //   must never read as "off", and a dead guard's stale clock must never
    //   read as a live countdown (plan/2026-10-23-0548/I2).
    // Scenario: FSD with early shutdown off; FSD with the guard dead; the
    //   guard dead while its clock file still holds an outage start.
    #[test]
    fn classify_priority_order() {
        let fsd = &[UpsStatusFlag::Ob, UpsStatusFlag::Fsd];
        assert_eq!(
            classify(&policy(None, None), &snapshot(fsd, None)),
            Some(ShutdownDisplay::ShuttingDown)
        );
        assert_eq!(
            classify(&both(), &snapshot(fsd, guard(false, timing(30)))),
            Some(ShutdownDisplay::ShuttingDown)
        );
        assert_eq!(
            classify(&both(), &snapshot(OB, guard(false, timing(30)))),
            Some(ShutdownDisplay::GuardDown { on_battery: true })
        );
    }

    // Intent: the clock file, not ups.status, decides whether a countdown
    //   runs: OL with a running clock still counts, until the guard clears it.
    // Why it exists: plan/2026-10-23-0548/D3 and AR1 -- the TUI must not
    //   re-derive the guard's "only OL stops the clock" rule.
    // Scenario: power returned one guard poll ago.
    #[test]
    fn classify_follows_clock_not_status() {
        let display = classify(&both(), &snapshot(OL, guard(true, timing(60)))).unwrap();
        assert!(
            matches!(display, ShutdownDisplay::Counting { .. }),
            "{display:?}"
        );
    }

    // Intent: overdue needs the deadline passed by more than the guard's
    //   trigger lag; inside the lag the timer still shows as counting at 0s.
    // Why it exists: the guard may take up to plan/2026-10-22-1122/I8's bound
    //   to act, so flagging earlier would cry wolf on every timed shutdown.
    // Scenario: on battery exactly at the deadline plus the lag bound.
    #[test]
    fn classify_overdue_waits_for_trigger_lag() {
        let at_bound = 300 + TRIGGER_LAG_BOUND.as_secs();
        let display = classify(&both(), &snapshot(OB, guard(true, timing(at_bound)))).unwrap();
        assert_eq!(display.column().0, "in 0s or <50%");
        let past = classify(&both(), &snapshot(OB, guard(true, timing(at_bound + 1)))).unwrap();
        assert_eq!(past.column().0, "overdue");
        // A timer near u64::MAX must not overflow the deadline.
        let huge = policy(Some(u64::MAX), None);
        let far = classify(&huge, &snapshot(OB, guard(true, timing(60)))).unwrap();
        assert!(matches!(far, ShutdownDisplay::Counting { .. }), "{far:?}");
    }

    // Intent: each trigger part renders only when that trigger is set.
    // Why it exists: plan/2026-10-23-0548/D4 -- naming a trigger the operator
    //   turned off would misstate when braid shuts down.
    // Scenario: timer only and floor only, armed and counting.
    #[test]
    fn classify_single_trigger_variants() {
        let timer = policy(Some(300), None);
        let floor = policy(None, Some(50));
        let counting = |p: &UpsShutdownPolicy| {
            text(&classify(p, &snapshot(OB, guard(true, timing(108)))).unwrap())
        };
        let armed = |p: &UpsShutdownPolicy| {
            text(&classify(p, &snapshot(OL, guard(true, GuardClock::Idle))).unwrap())
        };
        assert_eq!(
            counting(&timer),
            (
                "in 3m 12s".into(),
                Some("on battery 1m 48s -- braid shuts down in 3m 12s".into())
            )
        );
        assert_eq!(
            counting(&floor),
            (
                "below 50%".into(),
                Some("on battery 1m 48s -- braid shuts down below 50% charge (now 78%)".into())
            )
        );
        assert_eq!(armed(&timer), ("after 5m 0s".into(), None));
        assert_eq!(armed(&floor), ("below 50%".into(), None));
        // A floor-only clock never goes overdue: there is no deadline.
        let long = classify(&floor, &snapshot(OB, guard(true, timing(3600)))).unwrap();
        assert_eq!(long.column().0, "below 50%");
    }

    // Intent: the countdown turns red with under a minute left, yellow before.
    // Why it exists: plan/2026-10-23-0548/D4 -- the last minute is when the
    //   operator must act or let the host go.
    // Scenario: 61 s and 59 s left on a 5 min timer.
    #[test]
    fn classify_countdown_color_turns_red_under_a_minute() {
        let at = |elapsed| {
            let d = classify(&both(), &snapshot(OB, guard(true, timing(elapsed)))).unwrap();
            (d.column().1, d.banner().unwrap().1)
        };
        assert_eq!(at(239), (Color::Yellow, Color::Yellow));
        assert_eq!(at(241), (Color::Red, Color::Red));
    }

    // Intent: an unreadable charge says so in the banner.
    // Why it exists: plan/2026-10-23-0548/D4 and plan/2026-10-22-1122/I4 --
    //   an unknown charge must never read as above the floor.
    // Scenario: the UPS stopped reporting battery.charge mid-outage.
    #[test]
    fn classify_banner_says_charge_unknown() {
        let mut snap = snapshot(OB, guard(true, timing(108)));
        snap.battery_charge_pct = None;
        let banner = classify(&both(), &snap).unwrap().banner().unwrap().0;
        assert_eq!(
            banner,
            "on battery 1m 48s -- braid shuts down in 3m 12s, or below 50% charge (charge unknown)"
        );
    }

    // Intent: with early shutdown on but no guard reading, classify returns
    //   None rather than inventing a state.
    // Why it exists: plan/2026-10-23-0548/I2 -- the panic fallback snapshot
    //   knows nothing about the guard and must not render as armed.
    // Scenario: the UPS probe worker panicked.
    #[test]
    fn classify_without_guard_reading_is_none() {
        assert_eq!(classify(&both(), &snapshot(OB, None)), None);
    }
}
