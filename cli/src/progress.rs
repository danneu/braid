use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::parse::{BalanceState, ReplaceState, parse_btrfs_balance_status, parse_btrfs_replace_status};
use crate::types::MountPoint;
use std::io::Write;

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

/// Run a blocking btrfs command with progress polling.
/// Works for BtrfsBalanceRaid1, BtrfsBalanceSingle, and BtrfsDeviceRemove.
pub fn run_with_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    mount_point: &str,
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
                mount_point: MountPoint(mount_point.to_owned()),
            });
            if let Ok(ref raw) = poll
                && let Ok(status) = parse_btrfs_balance_status(raw) {
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

/// Run a blocking btrfs replace command with progress polling.
pub fn run_replace_with_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    mount_point: &str,
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
                mount_point: MountPoint(mount_point.to_owned()),
            });
            if let Ok(ref raw) = poll
                && let Ok(status) = parse_btrfs_replace_status(raw) {
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
            "/mnt/storage",
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
            "/mnt/storage",
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
            "/mnt/storage",
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
            "/mnt/storage",
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
            "/mnt/storage",
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
            "/mnt/storage",
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
            "/mnt/storage",
            ProgressOutput::Human,
        );
        let raw = result.unwrap();
        assert_eq!(raw.exit_status, 1);
        assert_eq!(raw.stderr, "replace failed");
    }
}
