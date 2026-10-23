//! `braid ups guard` -- shut the host down early during a long outage.
//!
//! Firmware low battery (LB) is the latest possible moment to shut down. The
//! guard polls the UPS and, once it has been on battery for
//! `braid.ups.shutdownAfter` or its charge drops below
//! `braid.ups.shutdownBelowCharge`, tells upsmon to force a shutdown. LB stays
//! the backstop. See plan/2026-10-22-1122 and ADR 020.
//!
//! The guard never powers the host off itself: it runs `upsmon -c fsd`, so
//! POWERDOWNFLAG, the FSD flag, and the ADR 018 unwinding all happen exactly
//! as they do on LB (plan/2026-10-22-1122/I1).

use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{ConfigError, UpsShutdownPolicy, config_read};
use crate::parse::types::{UpsSeverity, UpsStatusFlag, UpscOutput};
use crate::ups::query_ups;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Where the guard keeps its on-battery clock. The unit's RuntimeDirectory,
/// preserved across restarts and cleared at boot (plan/2026-10-22-1122/D10).
pub const CLOCK_PATH: &str = "/run/braid-ups-guard/clock";

/// Time between polls. Together with `QUERY_WAIT` it bounds detection lag
/// (plan/2026-10-22-1122/I8).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long one poll waits for `upsc` before treating the reading as failed.
/// Kept under `POLL_INTERVAL` so a hung query never delays a running timer by
/// more than one poll.
const QUERY_WAIT: Duration = Duration::from_secs(4);

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("UPS support is not enabled in the braid config")]
    UpsNotEnabled,
    #[error("no shutdown trigger is configured; the guard has nothing to do")]
    NoTrigger,
    #[error("cannot read CLOCK_BOOTTIME: {0}")]
    Clock(#[source] nix::Error),
}

/// One poll's view of the UPS. A failed query carries its reason only for
/// logging; the decision treats every failure the same way.
#[derive(Debug, Clone)]
pub enum Observation {
    Read(Box<UpscOutput>),
    Failed(String),
}

/// Why the guard is forcing a shutdown, with the values that tripped it, so
/// the journal line can name both (plan/2026-10-22-1122/I6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Timer {
        on_battery: Duration,
        limit: Duration,
    },
    ChargeFloor {
        charge_pct: u8,
        floor_pct: u8,
    },
}

impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trigger::Timer { on_battery, limit } => write!(
                f,
                "on battery for {}s, shutdownAfter is {}s",
                on_battery.as_secs(),
                limit.as_secs()
            ),
            Trigger::ChargeFloor {
                charge_pct,
                floor_pct,
            } => write!(
                f,
                "battery charge {charge_pct}% is below shutdownBelowCharge {floor_pct}%"
            ),
        }
    }
}

/// The result of one poll: the new clock and whether to force a shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// CLOCK_BOOTTIME reading at the first on-battery poll of this outage, or
    /// `None` while no outage is being timed.
    pub on_battery_since: Option<Duration>,
    pub trigger: Option<Trigger>,
    /// Whether `battery.charge` was readable on this poll. `None` when no
    /// floor is configured or the query failed, so it says nothing new.
    pub charge_readable: Option<bool>,
}

/// The whole policy, with no I/O, so every case is a unit test
/// (plan/2026-10-22-1122/D2).
///
/// The clock starts on the first reading whose status has `OB` and stops only
/// on the shared classifier's `online` verdict; a failed, empty, or unknown
/// reading leaves it as it was (plan/2026-10-22-1122/I2). The charge floor
/// needs a readable charge on this poll; an unreadable one never counts as
/// above the floor and never holds back the timer (plan/2026-10-22-1122/I4).
pub fn decide(
    observation: &Observation,
    since: Option<Duration>,
    now: Duration,
    policy: &UpsShutdownPolicy,
) -> Decision {
    let reading = match observation {
        Observation::Read(out) => Some(&**out),
        Observation::Failed(_) => None,
    };
    let since = match reading {
        Some(out) if out.severity() == UpsSeverity::Online => None,
        Some(out) if out.status_flags.contains(&UpsStatusFlag::Ob) => since.or(Some(now)),
        _ => since,
    };
    let charge = reading.and_then(|out| out.battery.charge_pct);
    let charge_readable = match (policy.below_charge_pct, reading) {
        (Some(_), Some(_)) => Some(charge.is_some()),
        _ => None,
    };

    let trigger = since.and_then(|start| {
        let on_battery = now.saturating_sub(start);
        let timer = policy
            .after_secs
            .map(|limit| Duration::from_secs(limit.get()))
            .filter(|limit| on_battery >= *limit)
            .map(|limit| Trigger::Timer { on_battery, limit });
        let floor = match (policy.below_charge_pct, charge) {
            (Some(floor), Some(pct)) if pct < floor.get() => Some(Trigger::ChargeFloor {
                charge_pct: pct,
                floor_pct: floor.get(),
            }),
            _ => None,
        };
        timer.or(floor)
    });

    Decision {
        on_battery_since: since,
        trigger,
        charge_readable,
    }
}

/// Reads CLOCK_BOOTTIME, which counts time spent suspended and restarts at
/// boot, so a host that auto-suspends on battery does not get a fresh timer
/// after resume (plan/2026-10-22-1122/D10).
pub fn boottime_now() -> Result<Duration, nix::Error> {
    #[cfg(target_os = "linux")]
    let id = nix::time::ClockId::CLOCK_BOOTTIME;
    // Non-Linux builds exist only for development; braid runs on NixOS.
    #[cfg(not(target_os = "linux"))]
    let id = nix::time::ClockId::CLOCK_MONOTONIC;
    let ts = nix::time::clock_gettime(id)?;
    Ok(Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32))
}

/// The persisted on-battery clock: one CLOCK_BOOTTIME reading in whole
/// seconds, or no file when no outage is being timed. It outlives a guard
/// restart because the unit preserves its runtime directory, and dies at boot
/// with `/run` (plan/2026-10-22-1122/I3).
pub struct ClockFile {
    path: PathBuf,
}

impl ClockFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A missing file means no running clock. An unreadable or corrupt one is
    /// returned as an error so the caller can say so; written atomically, it
    /// should never be corrupt.
    pub fn load(&self) -> Result<Option<Duration>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => text
                .trim()
                .parse::<u64>()
                .map(|secs| Some(Duration::from_secs(secs)))
                .map_err(|e| format!("corrupt clock file {}: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", self.path.display())),
        }
    }

    pub fn store(&self, since: Option<Duration>) -> Result<(), String> {
        let result = match since {
            Some(since) => crate::state_io::atomic_write(
                &self.path,
                format!("{}\n", since.as_secs()).as_bytes(),
            ),
            None => match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        result.map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
}

/// How bad a guard log line is. Errors and warnings go to stderr, the rest to
/// stdout; journald tags both with the unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// The guard's journal, as a seam so tests can read what it wrote.
pub trait GuardLog {
    fn line(&mut self, level: Level, message: &str);
}

/// Production log: one plain ASCII line per event.
pub struct StdLog;

impl GuardLog for StdLog {
    fn line(&mut self, level: Level, message: &str) {
        match level {
            Level::Info => println!("{message}"),
            Level::Warning => eprintln!("warning: {message}"),
            Level::Error => eprintln!("error: {message}"),
        }
    }
}

/// The guard's state between polls. Only the clock is persisted; the rest is
/// what one process needs to log changes once and to send FSD once.
pub struct Guard<R: CommandRunner + Send + 'static> {
    runner: Arc<R>,
    ups_name: String,
    policy: UpsShutdownPolicy,
    clock: ClockFile,
    since: Option<Duration>,
    /// A query still running from an earlier poll. Never more than one, so a
    /// hung `upsc` cannot pile up children.
    in_flight: Option<Receiver<Observation>>,
    /// FSD was sent for the current outage. Cleared when the clock stops.
    fsd_sent: bool,
    last_query_failed: bool,
    last_charge_readable: Option<bool>,
}

impl<R: CommandRunner + Send + 'static> Guard<R> {
    /// Picks up a clock left by an earlier guard process in this boot, so a
    /// restart keeps the original deadline.
    pub fn new(
        runner: Arc<R>,
        ups_name: String,
        policy: UpsShutdownPolicy,
        clock: ClockFile,
        log: &mut dyn GuardLog,
    ) -> Self {
        let since = clock.load().unwrap_or_else(|e| {
            log.line(Level::Error, &format!("{e}; starting with no clock"));
            None
        });
        if let Some(since) = since {
            log.line(
                Level::Info,
                &format!(
                    "resuming on-battery clock started at boot+{}s",
                    since.as_secs()
                ),
            );
        }
        Self {
            runner,
            ups_name,
            policy,
            clock,
            since,
            in_flight: None,
            fsd_sent: false,
            last_query_failed: false,
            last_charge_readable: None,
        }
    }

    /// One poll: read the UPS (waiting at most `wait`), decide, persist the
    /// clock, and force a shutdown if a trigger holds. `now` is read after the
    /// UPS answers so the reading and the time match.
    pub fn poll(&mut self, wait: Duration, now: impl FnOnce() -> Duration, log: &mut dyn GuardLog) {
        let observation = self.observe(wait);
        let now = now();
        self.note_query(&observation, log);

        let decision = decide(&observation, self.since, now, &self.policy);
        self.note_charge(decision.charge_readable, log);
        if decision.on_battery_since != self.since {
            match decision.on_battery_since {
                Some(_) => log.line(Level::Info, "UPS on battery: starting the shutdown clock"),
                None => log.line(Level::Info, "UPS back on utility power: clock stopped"),
            }
            if let Err(e) = self.clock.store(decision.on_battery_since) {
                log.line(Level::Error, &e);
            }
            self.since = decision.on_battery_since;
        }
        if self.since.is_none() {
            self.fsd_sent = false;
        }

        if let Some(trigger) = decision.trigger
            && !self.fsd_sent
        {
            log.line(
                Level::Info,
                &format!("forcing shutdown through upsmon: {trigger}"),
            );
            match self.runner.run(&CmdRequest::UpsmonFsd) {
                Ok(out) if out.exit_status == 0 => self.fsd_sent = true,
                Ok(out) => log.line(
                    Level::Error,
                    &format!(
                        "upsmon -c fsd failed: exit {}{}; retrying next poll",
                        out.exit_status,
                        crate::util::detail_suffix(out.stderr.trim())
                    ),
                ),
                Err(e) => log.line(
                    Level::Error,
                    &format!("upsmon -c fsd failed: {e}; retrying next poll"),
                ),
            }
        }
    }

    /// Collects this poll's reading. The query runs on its own thread so a
    /// hung `upsc` costs at most `wait`, after which the poll goes on with a
    /// failed reading and the clock it already has (plan/2026-10-22-1122/I8).
    fn observe(&mut self, wait: Duration) -> Observation {
        let rx = match self.in_flight.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = mpsc::channel();
                let runner = Arc::clone(&self.runner);
                let name = self.ups_name.clone();
                std::thread::spawn(move || {
                    let observation = match query_ups(&*runner, &name) {
                        Ok(q) => Observation::Read(Box::new(q.parsed)),
                        Err(e) => Observation::Failed(e.to_string()),
                    };
                    let _ = tx.send(observation);
                });
                rx
            }
        };
        match rx.recv_timeout(wait) {
            Ok(observation) => observation,
            Err(RecvTimeoutError::Timeout) => {
                self.in_flight = Some(rx);
                Observation::Failed(format!(
                    "upsc {} did not answer within {}s",
                    self.ups_name,
                    wait.as_secs()
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Observation::Failed("UPS query thread exited without a reading".to_owned())
            }
        }
    }

    /// Logs a query failure when it starts and when it clears, not on every
    /// poll, so a long upsd outage does not flood the journal.
    fn note_query(&mut self, observation: &Observation, log: &mut dyn GuardLog) {
        match observation {
            Observation::Failed(reason) if !self.last_query_failed => {
                log.line(
                    Level::Warning,
                    &format!("UPS query failed: {reason}; keeping the current clock"),
                );
                self.last_query_failed = true;
            }
            Observation::Read(_) if self.last_query_failed => {
                log.line(Level::Info, "UPS query recovered");
                self.last_query_failed = false;
            }
            _ => {}
        }
    }

    /// Warns when `battery.charge` becomes unreadable, and notes when it comes
    /// back (plan/2026-10-22-1122/I4).
    fn note_charge(&mut self, readable: Option<bool>, log: &mut dyn GuardLog) {
        let Some(readable) = readable else { return };
        if self.last_charge_readable == Some(readable) {
            return;
        }
        if !readable {
            log.line(
                Level::Warning,
                "UPS does not report a readable battery.charge; shutdownBelowCharge cannot fire",
            );
        } else if self.last_charge_readable.is_some() {
            log.line(Level::Info, "UPS battery.charge is readable again");
        }
        self.last_charge_readable = Some(readable);
    }
}

/// Entry for `braid ups guard`, run by `braid-ups-guard.service`. It loops
/// until killed, so it returns only on a setup error.
pub fn cmd_ups_guard<R: CommandRunner + Send + 'static>(
    runner: R,
    config_path: &Path,
) -> Result<std::convert::Infallible, GuardError> {
    let config = config_read(config_path)?;
    let ups = config.ups().ok_or(GuardError::UpsNotEnabled)?;
    if !ups.shutdown.is_enabled() {
        return Err(GuardError::NoTrigger);
    }
    // Fail at startup, where systemd shows it, rather than on the first poll.
    boottime_now().map_err(GuardError::Clock)?;

    let mut log = StdLog;
    let mut guard = Guard::new(
        Arc::new(runner),
        ups.name.as_str().to_owned(),
        ups.shutdown.clone(),
        ClockFile::new(CLOCK_PATH),
        &mut log,
    );
    loop {
        let started = Instant::now();
        guard.poll(
            QUERY_WAIT,
            || boottime_now().unwrap_or(Duration::ZERO),
            &mut log,
        );
        std::thread::sleep(POLL_INTERVAL.saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};
    use crate::config::ChargeFloorPct;
    use crate::parse::parse_upsc;
    use std::num::NonZeroU64;
    use std::sync::Mutex;

    fn policy(after: Option<u64>, floor: Option<u8>) -> UpsShutdownPolicy {
        UpsShutdownPolicy {
            after_secs: after.map(|s| NonZeroU64::new(s).unwrap()),
            below_charge_pct: floor.map(|p| ChargeFloorPct::try_from(p).unwrap()),
        }
    }

    fn read(upsc: &str) -> Observation {
        Observation::Read(Box::new(parse_upsc(upsc)))
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[derive(Default)]
    struct VecLog(Vec<(Level, String)>);

    impl GuardLog for VecLog {
        fn line(&mut self, level: Level, message: &str) {
            self.0.push((level, message.to_owned()));
        }
    }

    impl VecLog {
        fn count(&self, needle: &str) -> usize {
            self.0.iter().filter(|(_, m)| m.contains(needle)).count()
        }
    }

    // Intent: the clock starts on the first OB reading, keeps its start time
    //   while OB holds, and stops only on an `online` verdict.
    // Why it exists: plan/2026-10-22-1122/I2. Restarting the clock on each OB
    //   poll would never fire; stopping on anything short of OL would let a
    //   flapping or unreadable status reset a long outage.
    // Scenario: power fails at t=100, stays out to t=200, returns at t=210.
    #[test]
    fn clock_starts_on_ob_and_stops_only_on_online() {
        let p = policy(Some(300), None);
        let d = decide(&read("ups.status: OB\n"), None, secs(100), &p);
        assert_eq!(d.on_battery_since, Some(secs(100)));
        let d = decide(
            &read("ups.status: OB DISCHRG\n"),
            d.on_battery_since,
            secs(200),
            &p,
        );
        assert_eq!(d.on_battery_since, Some(secs(100)));
        assert_eq!(d.trigger, None);
        let d = decide(
            &read("ups.status: OL CHRG\n"),
            d.on_battery_since,
            secs(210),
            &p,
        );
        assert_eq!(d.on_battery_since, None);
    }

    // Intent: a failed query, an empty or missing status, an unknown token,
    //   COMMBAD, and LB without OB neither start a stopped clock nor stop a
    //   running one.
    // Why it exists: plan/2026-10-22-1122/I2 -- only OL proves power is back,
    //   and only OB proves it is gone. upsd hiccups mid-outage are common.
    // Scenario: each such reading arrives once with no clock, once with a
    //   clock running since t=100.
    #[test]
    fn unproven_readings_neither_start_nor_stop_the_clock() {
        let p = policy(Some(300), Some(50));
        let readings = [
            Observation::Failed("upsc ups: connection refused".into()),
            read("battery.charge: 80\n"),
            read("ups.status:\nbattery.charge: 80\n"),
            read("ups.status: WEIRD\n"),
            read("ups.status: COMMBAD\n"),
            read("ups.status: LB\nbattery.charge: 10\n"),
        ];
        for obs in &readings {
            let d = decide(obs, None, secs(150), &p);
            assert_eq!(d.on_battery_since, None, "{obs:?} started the clock");
            assert_eq!(d.trigger, None, "{obs:?}");
            let d = decide(obs, Some(secs(100)), secs(150), &p);
            assert_eq!(
                d.on_battery_since,
                Some(secs(100)),
                "{obs:?} stopped the clock"
            );
        }
    }

    // Intent: the timer fires once on-battery time reaches shutdownAfter,
    //   even when this poll's query failed.
    // Why it exists: plan/2026-10-22-1122/I2 and I8 -- a dead upsd mid-outage
    //   must not hold off a shutdown that is already due.
    // Scenario: clock started at t=100 with a 300 s limit; polls at 399, 400,
    //   and 450 (failed query).
    #[test]
    fn timer_fires_at_the_limit_even_on_a_failed_reading() {
        let p = policy(Some(300), None);
        let since = Some(secs(100));
        let d = decide(&read("ups.status: OB\n"), since, secs(399), &p);
        assert_eq!(d.trigger, None);
        let d = decide(&read("ups.status: OB\n"), since, secs(400), &p);
        assert_eq!(
            d.trigger,
            Some(Trigger::Timer {
                on_battery: secs(300),
                limit: secs(300)
            })
        );
        let d = decide(&Observation::Failed("hung".into()), since, secs(450), &p);
        assert!(matches!(d.trigger, Some(Trigger::Timer { .. })), "{d:?}");
    }

    // Intent: the charge floor fires only on a readable charge below the floor
    //   while the clock runs; missing or unparseable charge never fires it,
    //   never blocks the timer, and is reported as unreadable.
    // Why it exists: plan/2026-10-22-1122/I4. Treating a missing charge as 0
    //   would shut down on every outage for UPSes that do not report it;
    //   treating it as 100 would hide that the floor cannot work.
    // Scenario: floor 50 %; charges 49, 50, absent, "n/a"; then the same
    //   unreadable charge past a 300 s timer.
    #[test]
    fn charge_floor_needs_a_readable_charge_below_the_floor() {
        let p = policy(Some(300), Some(50));
        let since = Some(secs(100));
        let d = decide(
            &read("ups.status: OB\nbattery.charge: 49\n"),
            since,
            secs(110),
            &p,
        );
        assert_eq!(
            d.trigger,
            Some(Trigger::ChargeFloor {
                charge_pct: 49,
                floor_pct: 50
            })
        );
        assert_eq!(d.charge_readable, Some(true));
        let d = decide(
            &read("ups.status: OB\nbattery.charge: 50\n"),
            since,
            secs(110),
            &p,
        );
        assert_eq!(d.trigger, None);
        for upsc in ["ups.status: OB\n", "ups.status: OB\nbattery.charge: n/a\n"] {
            let d = decide(&read(upsc), since, secs(110), &p);
            assert_eq!(d.trigger, None, "{upsc}");
            assert_eq!(d.charge_readable, Some(false), "{upsc}");
            let d = decide(&read(upsc), since, secs(400), &p);
            assert!(matches!(d.trigger, Some(Trigger::Timer { .. })), "{upsc}");
        }
        // Low charge on utility power is a charging battery, not an outage.
        let d = decide(
            &read("ups.status: OL CHRG\nbattery.charge: 10\n"),
            None,
            secs(110),
            &p,
        );
        assert_eq!(d.trigger, None);
        // With no floor configured, charge readability is not tracked.
        let d = decide(
            &read("ups.status: OB\n"),
            since,
            secs(110),
            &policy(Some(300), None),
        );
        assert_eq!(d.charge_readable, None);
    }

    // Intent: the first OB reading can fire the floor at once, and when both
    //   triggers hold the shutdown still fires (naming the timer).
    // Why it exists: plan/2026-10-22-1122/D1 -- whichever comes first; a
    //   guard started mid-outage, or a deep-discharged battery, must not wait
    //   for a second poll.
    // Scenario: power fails with the battery at 30 %; later both conditions
    //   hold on the same poll.
    #[test]
    fn either_trigger_alone_or_both_force_shutdown() {
        let p = policy(Some(300), Some(50));
        let d = decide(
            &read("ups.status: OB\nbattery.charge: 30\n"),
            None,
            secs(5),
            &p,
        );
        assert_eq!(d.on_battery_since, Some(secs(5)));
        assert!(
            matches!(d.trigger, Some(Trigger::ChargeFloor { .. })),
            "{d:?}"
        );
        let d = decide(
            &read("ups.status: OB\nbattery.charge: 30\n"),
            Some(secs(5)),
            secs(305),
            &p,
        );
        assert!(matches!(d.trigger, Some(Trigger::Timer { .. })), "{d:?}");
        // Timer off: only the floor can fire.
        let d = decide(
            &read("ups.status: OB\nbattery.charge: 60\n"),
            Some(secs(5)),
            secs(9999),
            &policy(None, Some(50)),
        );
        assert_eq!(d.trigger, None);
    }

    // Intent: the trigger line names the reason and the values that tripped
    //   it, in ASCII.
    // Why it exists: plan/2026-10-22-1122/I6 -- the journal is how an
    //   operator learns why the NAS powered off early.
    // Scenario: one line per trigger kind.
    #[test]
    fn trigger_lines_name_reason_and_values() {
        let timer = Trigger::Timer {
            on_battery: secs(301),
            limit: secs(300),
        }
        .to_string();
        assert_eq!(timer, "on battery for 301s, shutdownAfter is 300s");
        let floor = Trigger::ChargeFloor {
            charge_pct: 49,
            floor_pct: 50,
        }
        .to_string();
        assert_eq!(floor, "battery charge 49% is below shutdownBelowCharge 50%");
        assert!(timer.is_ascii() && floor.is_ascii());
    }

    fn ob_runner() -> MockRunner {
        MockRunner::default().with_handler(|r| match r {
            CmdRequest::UpscQuery { .. } => Some(Ok(RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: "ups.status: OB\nbattery.charge: 90\n".into(),
                stderr: String::new(),
                exit_status: 0,
            })),
            _ => None,
        })
    }

    fn fsd_count(runner: &MockRunner) -> usize {
        runner
            .requests()
            .iter()
            .filter(|r| **r == CmdRequest::UpsmonFsd)
            .count()
    }

    // Intent: a failed `upsmon -c fsd` is logged as an error and sent again
    //   on the next poll; a successful one is not sent again.
    // Why it exists: plan/2026-10-22-1122/I7. Treating a failed FSD as done
    //   would leave the host draining to LB; resending after success would
    //   signal upsmon and write the trigger line on every poll until poweroff.
    // Scenario: upsmon is restarting when the timer expires, so the first
    //   FSD exits 1; the second succeeds.
    #[test]
    fn failed_fsd_retries_and_successful_fsd_is_sent_once() {
        let attempts = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&attempts);
        let runner = ob_runner().with_handler(move |r| match r {
            CmdRequest::UpsmonFsd => {
                let mut n = seen.lock().unwrap();
                *n += 1;
                Some(Ok(RawCommandOutput {
                    cmd: "upsmon -c fsd".into(),
                    stdout: String::new(),
                    stderr: if *n == 1 {
                        "no PID file".into()
                    } else {
                        String::new()
                    },
                    exit_status: if *n == 1 { 1 } else { 0 },
                }))
            }
            _ => None,
        });
        let dir = tempfile::tempdir().unwrap();
        let mut log = VecLog::default();
        let mut guard = Guard::new(
            Arc::new(runner.clone()),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(dir.path().join("guard/clock")),
            &mut log,
        );
        let wait = secs(5);
        guard.poll(wait, || secs(100), &mut log);
        assert_eq!(fsd_count(&runner), 0);
        guard.poll(wait, || secs(400), &mut log);
        assert_eq!(fsd_count(&runner), 1);
        assert_eq!(log.count("upsmon -c fsd failed: exit 1: no PID file"), 1);
        guard.poll(wait, || secs(405), &mut log);
        assert_eq!(fsd_count(&runner), 2);
        guard.poll(wait, || secs(410), &mut log);
        guard.poll(wait, || secs(415), &mut log);
        assert_eq!(fsd_count(&runner), 2, "FSD resent after success");
        assert_eq!(log.count("forcing shutdown through upsmon"), 2);
    }

    // Intent: a `upsc` that never answers costs one poll at most `wait`, and
    //   a timer already running still fires on that poll.
    // Why it exists: plan/2026-10-22-1122/I8. `query_ups` blocks until upsc
    //   exits; run inline, a hung upsd would freeze the loop and the host
    //   would drain to LB.
    // Scenario: clock running since t=100 with a 300 s limit; upsd hangs from
    //   t=390. Two polls go by with no answer; only one query is started.
    #[test]
    fn hung_query_does_not_hold_back_a_running_timer() {
        let (release, hold) = mpsc::channel::<()>();
        let hold = Arc::new(Mutex::new(hold));
        let hung = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&hung);
        let runner = ob_runner().with_handler(move |r| match r {
            CmdRequest::UpscQuery { .. } if flag.load(std::sync::atomic::Ordering::SeqCst) => {
                let _ = hold.lock().unwrap().recv();
                Some(Err(CmdError::Failed("upsc ups: released".into())))
            }
            CmdRequest::UpsmonFsd => Some(Ok(RawCommandOutput {
                cmd: "upsmon -c fsd".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            })),
            _ => None,
        });
        let dir = tempfile::tempdir().unwrap();
        let mut log = VecLog::default();
        let mut guard = Guard::new(
            Arc::new(runner.clone()),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(dir.path().join("guard/clock")),
            &mut log,
        );
        guard.poll(secs(5), || secs(100), &mut log);
        hung.store(true, std::sync::atomic::Ordering::SeqCst);

        let wait = Duration::from_millis(50);
        let started = Instant::now();
        guard.poll(wait, || secs(390), &mut log);
        assert_eq!(fsd_count(&runner), 0);
        guard.poll(wait, || secs(401), &mut log);
        assert!(
            started.elapsed() < secs(2),
            "polls blocked on the hung query"
        );
        assert_eq!(fsd_count(&runner), 1);
        let queries = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::UpscQuery { .. }))
            .count();
        assert_eq!(queries, 2, "a second upsc started while one was hung");
        assert_eq!(log.count("did not answer within"), 1);
        drop(release);
    }

    // Intent: a new guard process picks up the clock an earlier one stored,
    //   so a restart mid-outage keeps the original deadline; the clock file
    //   is removed when power returns.
    // Why it exists: plan/2026-10-22-1122/I3 -- a rebuild switch or crash
    //   during an outage must not grant a fresh shutdownAfter.
    // Scenario: outage starts at t=100; the guard restarts at t=350 and polls
    //   at t=400; later power returns.
    #[test]
    fn restarted_guard_keeps_the_original_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guard/clock");
        let runner = ob_runner().with_handler(|r| match r {
            CmdRequest::UpsmonFsd => Some(Ok(RawCommandOutput {
                cmd: "upsmon -c fsd".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            })),
            _ => None,
        });
        let mut log = VecLog::default();
        let mut first = Guard::new(
            Arc::new(runner.clone()),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(&path),
            &mut log,
        );
        first.poll(secs(5), || secs(100), &mut log);
        drop(first);

        let mut second = Guard::new(
            Arc::new(runner.clone()),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(&path),
            &mut log,
        );
        second.poll(secs(5), || secs(350), &mut log);
        assert_eq!(fsd_count(&runner), 0);
        second.poll(secs(5), || secs(400), &mut log);
        assert_eq!(fsd_count(&runner), 1);
        assert_eq!(
            log.count("resuming on-battery clock started at boot+100s"),
            1
        );

        let online = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            crate::test_fixtures::mock_ok("upsc ups", "ups.status: OL\n"),
        );
        let mut third = Guard::new(
            Arc::new(online),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(&path),
            &mut log,
        );
        third.poll(secs(5), || secs(500), &mut log);
        assert!(!path.exists(), "clock file left after power returned");
    }

    // Intent: the production clock is CLOCK_BOOTTIME, and the value it
    //   reads round-trips through the clock file.
    // Why it exists: plan/2026-10-22-1122/D10. CLOCK_MONOTONIC stops during
    //   suspend, so a host that auto-suspends on battery would get a fresh
    //   timer after resume; this pins the clock the guard uses.
    // Scenario: read the guard's clock beside the kernel's BOOTTIME and
    //   MONOTONIC clocks, store it, and load it back.
    #[test]
    fn production_clock_is_boottime_and_round_trips() {
        let guard_now = boottime_now().unwrap();
        #[cfg(target_os = "linux")]
        {
            let read = |id| {
                let ts = nix::time::clock_gettime(id).unwrap();
                Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32)
            };
            let before = read(nix::time::ClockId::CLOCK_BOOTTIME);
            let guard_now = boottime_now().unwrap();
            let after = read(nix::time::ClockId::CLOCK_BOOTTIME);
            assert!(before <= guard_now && guard_now <= after);
        }

        let dir = tempfile::tempdir().unwrap();
        let clock = ClockFile::new(dir.path().join("guard/clock"));
        assert_eq!(clock.load(), Ok(None));
        clock.store(Some(guard_now)).unwrap();
        assert_eq!(clock.load(), Ok(Some(secs(guard_now.as_secs()))));
        clock.store(None).unwrap();
        assert_eq!(clock.load(), Ok(None));
    }

    // Intent: a corrupt clock file is reported and the guard starts with no
    //   clock instead of crashing in a restart loop.
    // Why it exists: a guard that cannot start leaves only firmware LB.
    // Scenario: the clock file holds garbage.
    #[test]
    fn corrupt_clock_file_is_reported_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clock");
        std::fs::write(&path, "not a number\n").unwrap();
        let mut log = VecLog::default();
        let _guard = Guard::new(
            Arc::new(MockRunner::default()),
            "ups".into(),
            policy(Some(300), None),
            ClockFile::new(&path),
            &mut log,
        );
        assert_eq!(log.count("corrupt clock file"), 1);
    }
}
