use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::cmd::{CommandRunner, RawCommandOutput, RealRunner};
use crate::config::FanControl;
use crate::state_paths::StatePaths;
use crate::tui::event::Event;
use crate::tui::model::{DiskIdentity, FanSnapshot, UpsSnapshot};
use crate::types::MountPoint;

use std::time::Duration;

pub const FAN_PROBE_INTERVAL: Duration = Duration::from_secs(5);
pub const POOL_PROBE_INTERVAL: Duration = Duration::from_secs(10);
pub const UPS_PROBE_INTERVAL: Duration = Duration::from_secs(5);

pub enum Effect {
    ProbePool {
        mount_point: MountPoint,
        /// Membership-derived disk identity passed to the worker probe thread
        /// as a single bundle (names + by_id + luks_uuid + devid).
        disks: DiskIdentity,
        paths: StatePaths,
    },
    ProbeFan {
        sysfs_root: PathBuf,
        dev_root: PathBuf,
        disk_by_id: HashMap<String, String>,
        fan_control: FanControl,
    },
    ScheduleFanProbe {
        delay: Duration,
    },
    SchedulePoolProbe {
        delay: Duration,
    },
    /// Run `upsc <name>` through `query_ups` on a worker thread; raw stdout
    /// and parsed state become `Event::UpsProbeFinished`.
    ProbeUps {
        name: String,
    },
    ScheduleUpsProbe {
        delay: Duration,
    },
    /// Run a raw Browse-tab command on a worker thread and route stdout
    /// back through generation-checked TUI update.
    BrowseRunCommand {
        request: crate::cmd::CmdRequest,
        generation: u64,
    },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::ProbePool {
            mount_point,
            disks,
            paths,
        } => {
            spawn_worker(
                cmd_tx,
                move || {
                    let start = std::time::Instant::now();
                    let runner = RealRunner;
                    let fs = crate::filesystem::RealFilesystem;
                    let backing_path_resolver = crate::luks::RealBackingPathResolver;
                    let result = crate::tui::probe::probe_pool_for_tui(
                        &runner,
                        &fs,
                        &mount_point,
                        &disks,
                        &paths,
                        &backing_path_resolver,
                    );
                    let elapsed = start.elapsed();
                    Event::PoolProbeFinished(result, elapsed)
                },
                // Pool/browse fallbacks carry the panic message. `elapsed` is
                // measured inside `body`; `Duration::ZERO` on panic is fine --
                // it only feeds the footer's probe-duration display.
                |msg| {
                    Event::PoolProbeFinished(
                        Err(format!("pool probe panicked: {msg}")),
                        Duration::ZERO,
                    )
                },
            );
        }
        Effect::ProbeFan {
            sysfs_root,
            dev_root,
            disk_by_id,
            fan_control,
        } => {
            spawn_worker(
                cmd_tx,
                move || {
                    let runner = RealRunner;
                    let snapshot = crate::tui::probe::probe_fan_for_tui(
                        &runner,
                        &sysfs_root,
                        &dev_root,
                        &disk_by_id,
                        &fan_control,
                    );
                    Event::FanProbeFinished(snapshot)
                },
                // Asymmetry vs. pool/browse: fan/ups reset to an Unknown snapshot
                // with no message -- identical to a normal failed probe. These
                // probes are low-risk and the degraded render is honest.
                |_| Event::FanProbeFinished(FanSnapshot::unknown()),
            );
        }
        Effect::ScheduleFanProbe { delay } => {
            // Sleep-in-worker timer, deliberately not a main-loop deadline: with
            // only two 5s cadences the per-tick spawn is negligible, and routing
            // through spawn_worker keeps every execute_effect spawn behind one
            // panic-safe boundary. (A loop-side deadline would couple probe cadence
            // to the redraw timeout.) The sleep can't panic; the on-panic fallback
            // re-emits the tick only to honor that single-boundary invariant.
            spawn_worker(
                cmd_tx,
                move || {
                    thread::sleep(delay);
                    Event::PollFanRefresh
                },
                |_| Event::PollFanRefresh,
            );
        }
        Effect::SchedulePoolProbe { delay } => {
            // Sleep-in-worker like ScheduleFanProbe. The pool cadence is slower
            // because it runs the heavy smartctl+btrfs probe.
            spawn_worker(
                cmd_tx,
                move || {
                    thread::sleep(delay);
                    Event::PollPoolRefresh
                },
                |_| Event::PollPoolRefresh,
            );
        }
        Effect::ProbeUps { name } => {
            spawn_worker(
                cmd_tx,
                move || {
                    let runner = RealRunner;
                    let snapshot = crate::tui::probe::probe_ups_for_tui(&runner, &name);
                    Event::UpsProbeFinished(snapshot)
                },
                // Unknown snapshot on panic, like ProbeFan (no message).
                |_| Event::UpsProbeFinished(UpsSnapshot::unknown()),
            );
        }
        Effect::ScheduleUpsProbe { delay } => {
            // Sleep-in-worker like ScheduleFanProbe. PollUpsRefresh and
            // PollFanRefresh are distinct ticks -- do not collapse them.
            spawn_worker(
                cmd_tx,
                move || {
                    thread::sleep(delay);
                    Event::PollUpsRefresh
                },
                |_| Event::PollUpsRefresh,
            );
        }
        Effect::BrowseRunCommand {
            request,
            generation,
        } => {
            spawn_worker(
                cmd_tx,
                move || {
                    let runner = RealRunner;
                    let raw = match runner.run(&request) {
                        Ok(raw) => raw,
                        Err(e) => RawCommandOutput {
                            cmd: format!("{request:?}"),
                            stdout: String::new(),
                            stderr: format!("error: {e}"),
                            exit_status: 1,
                        },
                    };
                    Event::BrowseCommandFinished { raw, generation }
                },
                // `generation` is Copy, so both closures capture it. Carry the
                // panic message like the pool fallback.
                move |msg| Event::BrowseCommandFinished {
                    raw: RawCommandOutput {
                        cmd: String::new(),
                        stdout: String::new(),
                        stderr: format!("browse command panicked: {msg}"),
                        exit_status: 1,
                    },
                    generation,
                },
            );
        }
    }
}

thread_local! {
    /// Set on threads spawned by `spawn_worker`. The TUI panic hook reads this
    /// (via `in_caught_worker`) to decide whether to run ratatui's
    /// restore-then-print: false (main / input threads) -> restore + print;
    /// true (caught worker) -> do nothing, because `catch_unwind` here will
    /// recover and the live TUI must survive.
    static IN_CAUGHT_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is a `spawn_worker` thread whose panic will be
/// caught. The panic hook in `mod.rs` gates ratatui's `restore()` on this, so
/// the `Cell` stays encapsulated here rather than being read across the module
/// boundary.
pub(crate) fn in_caught_worker() -> bool {
    IN_CAUGHT_WORKER.with(Cell::get)
}

/// Extract a human-readable message from a caught panic payload so a worker
/// panic surfaces in the rendered error instead of being lost.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Spawn a TUI worker thread that always delivers exactly one terminal `Event`,
/// even if `body` panics. The strand-prevention invariant lives here so every
/// probe/scheduler worker shares one panic-safe boundary instead of trusting
/// each closure to reach its `tx.send`. Marks the thread as caught so the panic
/// hook leaves the live terminal alone.
fn spawn_worker<B, P>(cmd_tx: &mpsc::Sender<Event>, body: B, on_panic: P)
where
    B: FnOnce() -> Event + Send + 'static,
    P: FnOnce(String) -> Event + Send + 'static,
{
    let tx = cmd_tx.clone();
    thread::spawn(move || {
        IN_CAUGHT_WORKER.with(|f| f.set(true));
        // AssertUnwindSafe is sound here: all captures are plain data and the
        // thread is discarded after the send, so nothing observes a post-unwind
        // broken invariant.
        let event = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(ev) => ev,
            Err(payload) => on_panic(panic_message(&*payload)),
        };
        let _ = tx.send(event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: a worker whose body panics still delivers exactly one terminal
    //         Event (the on_panic fallback), and the panic message survives
    //         into that event.
    // Why it exists: tx.send is the last statement in every worker body, so a
    //                panic before it strands the subsystem forever (pool spins
    //                in Loading; fan/ups cadence never rearms). spawn_worker's
    //                catch_unwind is the net; this proves it fires, marks the
    //                thread caught (the in-body assert), and routes the payload
    //                through panic_message. Deleting the set(true) flips the
    //                fallback away from "boom" and fails here.
    // Scenario: a probe body hits an unexpected panic mid-session (e.g. a
    //           can't-happen .expect in probe.rs) instead of returning cleanly.
    #[test]
    fn panicking_worker_delivers_fallback_event_with_message() {
        let (tx, rx) = mpsc::channel();
        spawn_worker(
            &tx,
            || {
                assert!(in_caught_worker(), "worker thread must be marked caught");
                panic!("boom")
            },
            |msg| Event::PoolProbeFinished(Err(msg), Duration::ZERO),
        );
        // recv_timeout, never bare recv: a regression must fail, not hang.
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::PoolProbeFinished(Err(e), _)) => {
                assert!(
                    e.contains("boom"),
                    "fallback should carry the panic message, got: {e}"
                );
            }
            Ok(_) => panic!("expected PoolProbeFinished(Err(_)), got a different Event variant"),
            Err(_) => panic!("worker never delivered a fallback event within the timeout"),
        }
    }

    // Intent: a worker whose body returns normally delivers the body's event,
    //         not the on_panic fallback.
    // Why it exists: pairs with the panic test to pin that catch_unwind's Ok
    //                arm forwards the real result untouched -- a wrapper that
    //                always emitted the fallback would pass the panic test but
    //                break every healthy probe.
    // Scenario: the common case -- a probe completes without panicking.
    #[test]
    fn non_panicking_worker_delivers_body_event() {
        let (tx, rx) = mpsc::channel();
        spawn_worker(&tx, || Event::PollFanRefresh, |_| Event::PollUpsRefresh);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Event::PollFanRefresh)
        ));
    }
}
