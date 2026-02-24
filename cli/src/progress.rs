use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::parse::{parse_btrfs_balance_status, parse_btrfs_device_usage, BalanceState};
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

pub fn resolve_progress(mode: ProgressMode, stderr_is_tty: bool) -> bool {
    match mode {
        ProgressMode::Always => true,
        ProgressMode::Never => false,
        ProgressMode::Auto => stderr_is_tty,
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

pub fn format_remove_progress(current_bytes: u64, initial_bytes: u64) -> String {
    if initial_bytes == 0 {
        return "  remove: done".to_owned();
    }
    let moved = initial_bytes.saturating_sub(current_bytes);
    let pct_moved = (moved as f64 / initial_bytes as f64 * 100.0) as u64;
    format!(
        "  remove: {} remaining ({pct_moved}% moved)",
        format_bytes(current_bytes)
    )
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

fn clear_progress_line() {
    if is_stderr_tty() {
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();
    }
}

// ---------------------------------------------------------------------------
// Threaded runners
// ---------------------------------------------------------------------------

/// Run a blocking balance-like command with progress polling.
/// Works for BtrfsBalanceRaid1, BtrfsBalanceSingle, BtrfsDeviceRemoveMissing.
pub fn run_with_balance_progress<R: CommandRunner + Sync>(
    runner: &R,
    request: &CmdRequest,
    mount_point: &str,
) -> Result<RawCommandOutput, CmdError> {
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
                mount_point: mount_point.to_owned(),
            });
            if let Ok(ref raw) = poll {
                if let Ok(status) = parse_btrfs_balance_status(raw) {
                    let msg = match status.state {
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
                        } => format_balance_progress(
                            done_chunks,
                            estimated_total_chunks,
                            pct_left,
                        ),
                        BalanceState::None => continue,
                    };
                    if msg != last_msg {
                        write_progress_line(&msg);
                        last_msg = msg;
                    }
                }
            }
        }

        clear_progress_line();
        handle.join().expect("balance thread panicked")
    })
}

/// Run btrfs device remove with device-usage progress polling.
pub fn run_with_remove_progress<R: CommandRunner + Sync>(
    runner: &R,
    target: &str,
    mount_point: &str,
) -> Result<RawCommandOutput, CmdError> {
    // Capture initial used_bytes for target device
    let initial_bytes = get_device_used_bytes(runner, target, mount_point);

    let request = CmdRequest::BtrfsDeviceRemove {
        device: target.to_owned(),
        mount_point: mount_point.to_owned(),
    };

    // If we couldn't get initial bytes, just run without progress
    let Some(initial) = initial_bytes else {
        return runner.run(&request);
    };

    std::thread::scope(|s| {
        let handle = s.spawn(|| runner.run(&request));

        let mut last_msg = String::new();
        loop {
            if handle.is_finished() {
                break;
            }

            std::thread::sleep(std::time::Duration::from_secs(1));

            // Poll device usage
            if let Some(current) = get_device_used_bytes(runner, target, mount_point) {
                let msg = format_remove_progress(current, initial);
                if msg != last_msg {
                    write_progress_line(&msg);
                    last_msg = msg;
                }
            }
        }

        clear_progress_line();
        handle.join().expect("remove thread panicked")
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_device_used_bytes<R: CommandRunner>(
    runner: &R,
    target: &str,
    mount_point: &str,
) -> Option<u64> {
    let raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.to_owned(),
        })
        .ok()?;
    let usage = parse_btrfs_device_usage(&raw).ok()?;
    usage
        .devices
        .iter()
        .find(|d| d.path == target)
        .map(|d| d.used_bytes())
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
        assert_eq!(
            format_balance_progress(0, 0, 0),
            "  balance: waiting..."
        );
    }

    #[test]
    fn format_remove_progress_basic() {
        let initial = 1024 * 1024 * 1024; // 1 GiB
        let current = initial / 2;
        let msg = format_remove_progress(current, initial);
        assert!(msg.contains("512.0 MiB remaining"), "got: {msg}");
        assert!(msg.contains("50% moved"), "got: {msg}");
    }

    #[test]
    fn format_remove_progress_done() {
        let msg = format_remove_progress(0, 1000);
        assert!(msg.contains("100% moved"), "got: {msg}");
    }

    #[test]
    fn format_remove_progress_zero_initial() {
        assert_eq!(format_remove_progress(0, 0), "  remove: done");
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
        assert_eq!(
            format_bytes(1024 * 1024 * 1024 - 1),
            "1024.0 MiB"
        );
    }

    #[test]
    fn resolve_progress_auto_tty() {
        assert!(resolve_progress(ProgressMode::Auto, true));
    }

    #[test]
    fn resolve_progress_auto_no_tty() {
        assert!(!resolve_progress(ProgressMode::Auto, false));
    }

    #[test]
    fn resolve_progress_always_overrides() {
        assert!(resolve_progress(ProgressMode::Always, false));
    }

    #[test]
    fn resolve_progress_never_overrides() {
        assert!(!resolve_progress(ProgressMode::Never, true));
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
                mount_point: "/mnt/storage".to_owned(),
            },
            ok_raw("btrfs balance start", ""),
        );

        let result = run_with_balance_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: "/mnt/storage".to_owned(),
            },
            "/mnt/storage",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn balance_progress_poll_parse_failure_is_silent() {
        // BtrfsBalanceStatus returns garbage — progress failure is silent.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw("btrfs balance start", ""),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw("btrfs balance status", "garbage output here"),
            );

        let result = run_with_balance_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: "/mnt/storage".to_owned(),
            },
            "/mnt/storage",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn remove_progress_missing_target_entry() {
        // BtrfsDeviceUsageRaw returns valid output but target device isn't in it.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-other, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Unallocated:          536870912\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceRemove {
                    device: "/dev/mapper/braid-target".to_owned(),
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw("btrfs device remove", ""),
            );

        let result = run_with_remove_progress(
            &runner,
            "/dev/mapper/braid-target",
            "/mnt/storage",
        );
        // Should succeed — missing target in usage just means no progress shown.
        assert!(result.is_ok());
    }

    #[test]
    fn balance_progress_action_failure_propagation() {
        // BtrfsBalanceRaid1 returns exit_status=1 — error must propagate unchanged.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: "/mnt/storage".to_owned(),
            },
            err_raw("btrfs balance start", 1, "balance failed"),
        );

        let result = run_with_balance_progress(
            &runner,
            &CmdRequest::BtrfsBalanceRaid1 {
                mount_point: "/mnt/storage".to_owned(),
            },
            "/mnt/storage",
        );
        // The raw result has exit_status=1, but CmdError is not returned for non-zero exits.
        // The RawCommandOutput is returned as Ok with exit_status=1.
        let raw = result.unwrap();
        assert_eq!(raw.exit_status, 1);
        assert_eq!(raw.stderr, "balance failed");
    }
}
