use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::parse::{
    BalanceState, ReplaceState, parse_btrfs_balance_status, parse_btrfs_replace_status,
};
use crate::types::MountPoint;
use std::io::Write;
use std::time::Duration;

pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub trait Sleeper: Sync {
    fn sleep(&self, duration: Duration);
}

pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
pub(crate) struct NoopSleeper;

#[cfg(test)]
impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

pub(crate) trait ProgressSink: Sync {
    fn write_line(&self, msg: &str);
    fn write_json(&self, msg: &str);
    fn clear(&self);
}

pub(crate) struct StderrSink;

impl ProgressSink for StderrSink {
    fn write_line(&self, msg: &str) {
        write_progress_line(msg);
    }

    fn write_json(&self, msg: &str) {
        write_progress_json(msg);
    }

    fn clear(&self) {
        clear_progress_line();
    }
}

// ---------------------------------------------------------------------------
// ProgressMode (shared by init-disk and apply)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProgressMode {
    Auto,
    Always,
    Never,
}

// ---------------------------------------------------------------------------
// ProgressOutput (resolved from ProgressMode + json flag)
// ---------------------------------------------------------------------------

/// Resolved from ProgressMode + json flag. Plumbed through execute functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressOutput {
    Off,
    Human,
    Json,
}

pub fn resolve_progress_output(
    mode: ProgressMode,
    stderr_is_tty: bool,
    json: bool,
) -> ProgressOutput {
    match mode {
        ProgressMode::Never => ProgressOutput::Off,
        ProgressMode::Always => {
            if json {
                ProgressOutput::Json
            } else {
                ProgressOutput::Human
            }
        }
        ProgressMode::Auto => {
            if stderr_is_tty {
                if json {
                    ProgressOutput::Json
                } else {
                    ProgressOutput::Human
                }
            } else {
                ProgressOutput::Off
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting (pure, testable)
// ---------------------------------------------------------------------------

pub fn format_balance_progress(done: u64, total: u64, pct_left: u8) -> String {
    if total == 0 {
        return "  balance: waiting...".to_owned();
    }
    let pct_complete = 100u8.saturating_sub(pct_left);
    format!("  balance: {done}/{total} chunks ({pct_complete}% complete)")
}

pub fn format_balance_progress_json(done: u64, total: u64, pct_left: u8) -> String {
    format!(
        r#"{{"event":"progress","done_chunks":{done},"estimated_total_chunks":{total},"pct_left":{pct_left}}}"#
    )
}

pub fn format_replace_progress(pct: f64) -> String {
    format!("  replace: {pct:.1}% done")
}

pub fn format_replace_progress_json(pct: f64) -> String {
    format!(r#"{{"event":"progress","operation":"replace","pct_done":{pct:.1}}}"#)
}

pub(crate) fn format_device_remove_heartbeat(elapsed: Duration) -> String {
    format!("  device remove: working ({}s elapsed)", elapsed.as_secs())
}

pub(crate) fn format_device_remove_heartbeat_json(elapsed: Duration) -> String {
    format!(
        r#"{{"event":"device_remove_heartbeat","elapsed_secs":{}}}"#,
        elapsed.as_secs()
    )
}

pub fn pct_from_bytes(done: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    let pct = (u128::from(done) * 100) / u128::from(total);
    Some(pct.min(100) as u8)
}

pub fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn is_stderr_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn write_progress_line(msg: &str) {
    let mut stderr = std::io::stderr().lock();
    if is_stderr_tty() {
        // \r + clear to end of line + message
        let _ = write!(stderr, "\r\x1b[K{msg}");
    } else {
        let _ = writeln!(stderr, "{msg}");
    }
    let _ = stderr.flush();
}

fn write_progress_json(json: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{json}");
    let _ = stderr.flush();
}

fn clear_progress_line() {
    if is_stderr_tty() {
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();
    }
}

// ---------------------------------------------------------------------------
// Threaded runner
// ---------------------------------------------------------------------------

/// Run a blocking balance-driven btrfs command with progress
/// polling via `btrfs balance status`.
/// Works for BtrfsBalanceRaid1, BtrfsBalanceSingle, and the
/// BtrfsBalance* variants. NOT suitable for BtrfsDeviceRemove --
/// device remove uses its own exclusive-op path and does not
/// surface in balance status; route those through
/// `run_device_remove_with_progress` instead.
pub fn run_with_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    mount_point: &MountPoint,
    output: ProgressOutput,
) -> Result<RawCommandOutput, CmdError> {
    if output == ProgressOutput::Off {
        return runner.run(request);
    }

    std::thread::scope(|s| {
        let handle = s.spawn(|| runner.run(request));

        let mut last_msg = String::new();
        loop {
            // Check if the thread is done (non-blocking)
            if handle.is_finished() {
                break;
            }

            std::thread::sleep(std::time::Duration::from_secs(1));

            // Poll balance status
            let poll = runner.run(&CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point.clone(),
            });
            if let Ok(ref raw) = poll
                && let Ok(status) = parse_btrfs_balance_status(raw)
            {
                match status.state {
                    BalanceState::Running {
                        done_chunks,
                        estimated_total_chunks,
                        pct_left,
                        ..
                    }
                    | BalanceState::Paused {
                        done_chunks,
                        estimated_total_chunks,
                        pct_left,
                        ..
                    } => match output {
                        ProgressOutput::Human => {
                            let msg = format_balance_progress(
                                done_chunks,
                                estimated_total_chunks,
                                pct_left,
                            );
                            if msg != last_msg {
                                write_progress_line(&msg);
                                last_msg = msg;
                            }
                        }
                        ProgressOutput::Json => {
                            let json = format_balance_progress_json(
                                done_chunks,
                                estimated_total_chunks,
                                pct_left,
                            );
                            write_progress_json(&json);
                        }
                        ProgressOutput::Off => unreachable!(),
                    },
                    BalanceState::None => continue,
                }
            }
        }

        if output == ProgressOutput::Human {
            clear_progress_line();
        }
        handle.join().expect("command thread panicked")
    })
}

pub(crate) fn run_device_remove_with_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    output: ProgressOutput,
) -> Result<RawCommandOutput, CmdError> {
    run_device_remove_with_progress_using(runner, request, output, &RealSleeper, &StderrSink)
}

pub(crate) fn run_device_remove_with_progress_using<R, S, W>(
    runner: &R,
    request: &CmdRequest,
    output: ProgressOutput,
    sleeper: &S,
    sink: &W,
) -> Result<RawCommandOutput, CmdError>
where
    R: CommandRunner + Sync,
    S: Sleeper + ?Sized,
    W: ProgressSink + ?Sized,
{
    if output == ProgressOutput::Off {
        return runner.run(request);
    }

    std::thread::scope(|s| {
        let handle = s.spawn(|| runner.run(request));

        let mut elapsed = Duration::ZERO;
        let mut last_msg = String::new();
        loop {
            if handle.is_finished() {
                break;
            }

            sleeper.sleep(HEARTBEAT_INTERVAL);
            elapsed += HEARTBEAT_INTERVAL;

            if handle.is_finished() {
                break;
            }

            match output {
                ProgressOutput::Human => {
                    let msg = format_device_remove_heartbeat(elapsed);
                    if msg != last_msg {
                        sink.write_line(&msg);
                        last_msg = msg;
                    }
                }
                ProgressOutput::Json => {
                    sink.write_json(&format_device_remove_heartbeat_json(elapsed));
                }
                ProgressOutput::Off => unreachable!(),
            }
        }

        if output == ProgressOutput::Human {
            sink.clear();
        }
        handle.join().expect("command thread panicked")
    })
}

/// Run a blocking btrfs replace command with progress polling.
pub fn run_replace_with_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    mount_point: &MountPoint,
    output: ProgressOutput,
) -> Result<RawCommandOutput, CmdError> {
    if output == ProgressOutput::Off {
        return runner.run(request);
    }

    std::thread::scope(|s| {
        let handle = s.spawn(|| runner.run(request));

        let mut last_msg = String::new();
        loop {
            if handle.is_finished() {
                break;
            }

            std::thread::sleep(std::time::Duration::from_secs(1));

            let poll = runner.run(&CmdRequest::BtrfsReplaceStatus {
                mount_point: mount_point.clone(),
            });
            if let Ok(ref raw) = poll
                && let Ok(status) = parse_btrfs_replace_status(raw)
            {
                match status.state {
                    ReplaceState::Running { pct } => match output {
                        ProgressOutput::Human => {
                            let msg = format_replace_progress(pct);
                            if msg != last_msg {
                                write_progress_line(&msg);
                                last_msg = msg;
                            }
                        }
                        ProgressOutput::Json => {
                            let json = format_replace_progress_json(pct);
                            write_progress_json(&json);
                        }
                        ProgressOutput::Off => unreachable!(),
                    },
                    ReplaceState::Finished | ReplaceState::None => continue,
                }
            }
        }

        if output == ProgressOutput::Human {
            clear_progress_line();
        }
        handle.join().expect("command thread panicked")
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    // --- Pure formatting tests ---

    #[test]
    fn format_balance_progress_basic() {
        assert_eq!(
            format_balance_progress(3, 10, 70),
            "  balance: 3/10 chunks (30% complete)"
        );
    }

    #[test]
    fn format_balance_progress_done() {
        assert_eq!(
            format_balance_progress(10, 10, 0),
            "  balance: 10/10 chunks (100% complete)"
        );
    }

    #[test]
    fn format_balance_progress_zero_total() {
        assert_eq!(format_balance_progress(0, 0, 0), "  balance: waiting...");
    }

    #[test]
    fn format_balance_progress_json_basic() {
        assert_eq!(
            format_balance_progress_json(3, 10, 70),
            r#"{"event":"progress","done_chunks":3,"estimated_total_chunks":10,"pct_left":70}"#
        );
    }

    #[test]
    fn format_device_remove_heartbeat_basic() {
        assert_eq!(
            format_device_remove_heartbeat(Duration::ZERO),
            "  device remove: working (0s elapsed)"
        );
        assert_eq!(
            format_device_remove_heartbeat(Duration::from_secs(7)),
            "  device remove: working (7s elapsed)"
        );
        assert_eq!(
            format_device_remove_heartbeat(Duration::from_secs(120)),
            "  device remove: working (120s elapsed)"
        );
    }

    #[test]
    fn format_device_remove_heartbeat_json_basic() {
        assert_eq!(
            format_device_remove_heartbeat_json(Duration::from_secs(7)),
            r#"{"event":"device_remove_heartbeat","elapsed_secs":7}"#
        );
    }

    #[test]
    fn format_bytes_gib() {
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn format_bytes_mib() {
        assert_eq!(format_bytes(512 * 1024 * 1024), "512.0 MiB");
    }

    #[test]
    fn format_bytes_below_gib_threshold() {
        // Just under 1 GiB should show MiB
        assert_eq!(format_bytes(1024 * 1024 * 1024 - 1), "1024.0 MiB");
    }

    /*
     * Intent: byte progress percentage uses integer truncation.
     * Why: status JSON and idle output expose whole-number scrub progress.
     * Scenario: btrfs reports 45 scrubbed bytes out of 100 total bytes.
     */
    #[test]
    fn pct_from_bytes_truncates() {
        assert_eq!(pct_from_bytes(45, 100), Some(45));
    }

    /*
     * Intent: byte progress percentage is absent when total bytes is zero.
     * Why: division by zero should degrade to unavailable progress.
     * Scenario: btrfs reports a running scrub before total work is known.
     */
    #[test]
    fn pct_from_bytes_zero_total_is_none() {
        assert_eq!(pct_from_bytes(1, 0), None);
    }

    /*
     * Intent: byte progress percentage does not overflow on huge counters.
     * Why: multiplying u64 byte counters by 100 directly can panic in debug
     * builds before division.
     * Scenario: btrfs reports very large scrubbed and total byte counters.
     */
    #[test]
    fn pct_from_bytes_handles_large_values() {
        assert_eq!(pct_from_bytes(u64::MAX, u64::MAX), Some(100));
    }

    /*
     * Intent: byte progress percentage clamps impossible over-complete values.
     * Why: callers expose the value as u8 progress and must not wrap.
     * Scenario: btrfs reports scrubbed bytes greater than total bytes.
     */
    #[test]
    fn pct_from_bytes_clamps_above_100() {
        assert_eq!(pct_from_bytes(300, 100), Some(100));
    }

    // --- resolve_progress_output tests ---

    #[test]
    fn resolve_progress_output_never_returns_off() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Never, true, false),
            ProgressOutput::Off
        );
        assert_eq!(
            resolve_progress_output(ProgressMode::Never, true, true),
            ProgressOutput::Off
        );
        assert_eq!(
            resolve_progress_output(ProgressMode::Never, false, false),
            ProgressOutput::Off
        );
    }

    #[test]
    fn resolve_progress_output_always_human() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Always, false, false),
            ProgressOutput::Human
        );
    }

    #[test]
    fn resolve_progress_output_always_json() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Always, false, true),
            ProgressOutput::Json
        );
    }

    #[test]
    fn resolve_progress_output_auto_tty_human() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Auto, true, false),
            ProgressOutput::Human
        );
    }

    #[test]
    fn resolve_progress_output_auto_tty_json() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Auto, true, true),
            ProgressOutput::Json
        );
    }

    #[test]
    fn resolve_progress_output_auto_no_tty_off() {
        assert_eq!(
            resolve_progress_output(ProgressMode::Auto, false, false),
            ProgressOutput::Off
        );
        assert_eq!(
            resolve_progress_output(ProgressMode::Auto, false, true),
            ProgressOutput::Off
        );
    }

    // --- Threaded behavior tests ---

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn device_remove_request() -> CmdRequest {
        CmdRequest::BtrfsDeviceRemove {
            device: "/dev/mapper/braid-disk2".to_owned(),
            mount_point: mp(),
        }
    }

    #[derive(Default)]
    struct RemoveGate {
        released: bool,
        done: bool,
    }

    #[derive(Clone)]
    struct BlockingRemoveRunner {
        gate: Arc<(Mutex<RemoveGate>, Condvar)>,
        calls: Arc<AtomicUsize>,
    }

    impl BlockingRemoveRunner {
        fn new() -> Self {
            Self {
                gate: Arc::new((Mutex::new(RemoveGate::default()), Condvar::new())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn gate(&self) -> Arc<(Mutex<RemoveGate>, Condvar)> {
            Arc::clone(&self.gate)
        }
    }

    impl CommandRunner for BlockingRemoveRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    let (lock, cvar) = &*self.gate;
                    let mut state = lock.lock().unwrap();
                    while !state.released {
                        state = cvar.wait(state).unwrap();
                    }
                    state.done = true;
                    cvar.notify_all();
                    Ok(ok_raw("btrfs device remove", ""))
                }
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    #[derive(Clone, Default)]
    struct FakeSleeper {
        calls: Arc<Mutex<Vec<Duration>>>,
    }

    impl FakeSleeper {
        fn calls(&self) -> Vec<Duration> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Sleeper for FakeSleeper {
        fn sleep(&self, duration: Duration) {
            self.calls.lock().unwrap().push(duration);
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSink {
        lines: Arc<Mutex<Vec<String>>>,
        jsons: Arc<Mutex<Vec<String>>>,
        clears: Arc<AtomicUsize>,
        gate: Option<Arc<(Mutex<RemoveGate>, Condvar)>>,
    }

    impl RecordingSink {
        fn with_gate(gate: Arc<(Mutex<RemoveGate>, Condvar)>) -> Self {
            Self {
                gate: Some(gate),
                ..Self::default()
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }

        fn jsons(&self) -> Vec<String> {
            self.jsons.lock().unwrap().clone()
        }

        fn clears(&self) -> usize {
            self.clears.load(Ordering::SeqCst)
        }

        fn release_worker_and_wait(&self) {
            let Some(gate) = &self.gate else {
                return;
            };
            let (lock, cvar) = &**gate;
            let mut state = lock.lock().unwrap();
            state.released = true;
            cvar.notify_all();
            while !state.done {
                state = cvar.wait(state).unwrap();
            }
        }
    }

    impl ProgressSink for RecordingSink {
        fn write_line(&self, msg: &str) {
            self.lines.lock().unwrap().push(msg.to_owned());
            self.release_worker_and_wait();
        }

        fn write_json(&self, msg: &str) {
            self.jsons.lock().unwrap().push(msg.to_owned());
            self.release_worker_and_wait();
        }

        fn clear(&self) {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    /*
     * Intent: the device-remove progress helper emits a human heartbeat
     * while the worker is running and clears on completion.
     * Why it exists: the original bug was that `btrfs device remove`
     * produced no operator output on slow pools.
     * Scenario: a mock device remove blocks until the first heartbeat
     * reaches the sink, then returns.
     */
    #[test]
    fn device_remove_emits_heartbeat_human() {
        let runner = BlockingRemoveRunner::new();
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::with_gate(runner.gate());

        let result = run_device_remove_with_progress_using(
            &runner,
            &device_remove_request(),
            ProgressOutput::Human,
            &sleeper,
            &sink,
        );

        assert!(result.is_ok(), "device remove should succeed: {result:?}");
        let lines = sink.lines();
        assert!(
            !lines.is_empty(),
            "expected at least one heartbeat line to be written"
        );
        assert_eq!(lines[0], format_device_remove_heartbeat(HEARTBEAT_INTERVAL));
        assert_eq!(sink.jsons(), Vec::<String>::new());
        assert_eq!(sink.clears(), 1, "human progress should clear once");
    }

    /*
     * Intent: JSON progress mode emits device-remove heartbeat events.
     * Why it exists: JSON mode must not silently drop the liveness signal.
     * Scenario: same blocked device remove as the human-mode test, but
     * using the newline-delimited JSON progress pathway.
     */
    #[test]
    fn device_remove_emits_heartbeat_json() {
        let runner = BlockingRemoveRunner::new();
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::with_gate(runner.gate());

        let result = run_device_remove_with_progress_using(
            &runner,
            &device_remove_request(),
            ProgressOutput::Json,
            &sleeper,
            &sink,
        );

        assert!(result.is_ok(), "device remove should succeed: {result:?}");
        let jsons = sink.jsons();
        assert!(
            !jsons.is_empty(),
            "expected at least one heartbeat JSON event to be written"
        );
        assert_eq!(
            jsons[0],
            format_device_remove_heartbeat_json(HEARTBEAT_INTERVAL)
        );
        assert_eq!(sink.lines(), Vec::<String>::new());
        assert_eq!(sink.clears(), 0, "JSON progress should not clear");
    }

    /*
     * Intent: ProgressOutput::Off short-circuits to runner.run with no
     * heartbeat emission.
     * Why it exists: callers use Off as the quiet escape hatch.
     * Scenario: a successful device remove runs without touching the
     * injected sleeper or sink.
     */
    #[test]
    fn device_remove_off_emits_nothing() {
        let runner = MockRunner::default()
            .with_output(device_remove_request(), ok_raw("btrfs device remove", ""));
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::default();

        let result = run_device_remove_with_progress_using(
            &runner,
            &device_remove_request(),
            ProgressOutput::Off,
            &sleeper,
            &sink,
        );

        assert!(result.is_ok(), "device remove should succeed: {result:?}");
        assert_eq!(sink.lines(), Vec::<String>::new());
        assert_eq!(sink.jsons(), Vec::<String>::new());
        assert_eq!(sink.clears(), 0);
        assert!(
            sleeper.calls().is_empty(),
            "Off mode must not call the sleeper"
        );
    }

    /*
     * Intent: the device-remove helper asks the Sleeper for the configured
     * heartbeat interval.
     * Why it exists: the cadence is user-visible and should not drift
     * silently.
     * Scenario: a blocked device remove runs through one heartbeat tick.
     */
    #[test]
    fn device_remove_sleeps_at_configured_interval() {
        let runner = BlockingRemoveRunner::new();
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::with_gate(runner.gate());

        let result = run_device_remove_with_progress_using(
            &runner,
            &device_remove_request(),
            ProgressOutput::Human,
            &sleeper,
            &sink,
        );

        assert!(result.is_ok(), "device remove should succeed: {result:?}");
        let calls = sleeper.calls();
        assert!(
            calls.iter().any(|d| *d == HEARTBEAT_INTERVAL),
            "expected a sleep call for {HEARTBEAT_INTERVAL:?}, got {calls:?}"
        );
    }

    #[test]
    fn balance_progress_fast_finish_before_first_poll() {
        // MockRunner returns instantly — command finishes before any poll happens.
        // Only seed the balance command, no status mock needed.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("btrfs balance start", ""),
        );

        let result = run_with_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn balance_progress_poll_parse_failure_is_silent() {
        // BtrfsBalanceStatus returns garbage — progress failure is silent.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("btrfs balance start", ""),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("btrfs balance status", "garbage output here"),
            );

        let result = run_with_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn balance_progress_action_failure_propagation() {
        // BtrfsBalanceRaid1 returns exit_status=1 — error must propagate unchanged.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("btrfs balance start", 1, "balance failed"),
        );

        let result = run_with_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        // The raw result has exit_status=1, but CmdError is not returned for non-zero exits.
        // The RawCommandOutput is returned as Ok with exit_status=1.
        let raw = result.unwrap();
        assert_eq!(raw.exit_status, 1);
        assert_eq!(raw.stderr, "balance failed");
    }

    #[test]
    fn progress_off_runs_without_thread() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("btrfs balance start", ""),
        );

        let result = run_with_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Off,
        );
        assert!(result.is_ok());
    }

    // --- Replace progress tests ---

    #[test]
    fn format_replace_progress_basic() {
        assert_eq!(format_replace_progress(45.3), "  replace: 45.3% done");
    }

    #[test]
    fn format_replace_progress_json_basic() {
        assert_eq!(
            format_replace_progress_json(45.3),
            r#"{"event":"progress","operation":"replace","pct_done":45.3}"#
        );
    }

    #[test]
    fn replace_progress_fast_finish_before_first_poll() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("btrfs replace start", ""),
        );

        let result = run_replace_with_progress(
            &runner,
            &CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn replace_progress_poll_parse_failure_is_silent() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsReplaceStart {
                    devid: 2,
                    target_device: "/dev/mapper/new".to_owned(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("btrfs replace start", ""),
            )
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("btrfs replace status", "garbage output here"),
            );

        let result = run_replace_with_progress(
            &runner,
            &CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn replace_progress_action_failure_propagation() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("btrfs replace start", 1, "replace failed"),
        );

        let result = run_replace_with_progress(
            &runner,
            &CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            &mp(),
            ProgressOutput::Human,
        );
        let raw = result.unwrap();
        assert_eq!(raw.exit_status, 1);
        assert_eq!(raw.stderr, "replace failed");
    }
}
