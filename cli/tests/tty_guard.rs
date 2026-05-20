#![cfg(unix)]

use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn maybe_run_probe() {
    let Ok(probe) = std::env::var("BRAID_TTY_GUARD_PROBE") else {
        return;
    };

    detach_session();
    redirect_stdio_to_dev_null();
    let result: std::io::Result<()> = match probe.as_str() {
        "tui" => {
            let paths = braid_cli::state_paths::StatePaths::production();
            braid_cli::tui::run(Path::new("/etc/nonexistent"), &paths)
        }
        "tui_demo" => braid_cli::tui::run_demo(),
        other => panic!("unknown BRAID_TTY_GUARD_PROBE value: {other}"),
    };
    let err = result.expect_err("expected TTY guard to reject redirected stdio");
    let msg = err.to_string();
    assert!(
        msg.contains("requires a terminal"),
        "expected TTY guard error, got: {msg}"
    );
    std::process::exit(0);
}

fn detach_session() {
    nix::unistd::setsid().expect("setsid");
}

fn redirect_stdio_to_dev_null() {
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null");

    nix::unistd::dup2_stdin(&null).expect("dup2 stdin");
    nix::unistd::dup2_stdout(&null).expect("dup2 stdout");
}

fn run_child_probe(probe: &str, parent_test_name: &str) {
    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .env("BRAID_TTY_GUARD_PROBE", probe)
        .arg("--exact")
        .arg(parent_test_name)
        .arg("--test-threads=1")
        .spawn()
        .expect("spawn tty guard probe");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("wait probe") {
            assert!(
                status.success(),
                "tty guard probe {probe} exited with {status} -- the guard regressed: \
                 either ratatui::init was reached with redirected stdio or the returned \
                 io::Error did not contain \"requires a terminal\""
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "tty guard probe {probe} wedged for 5s -- the guard regressed: \
                 ratatui::init reached the /dev/tty fallback path before require_tty"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Intent: braid_cli::tui::run rejects redirected stdio with an io::Error
// before ratatui::init() runs.
// Why it exists: protects the main `braid tui` interactive surface from the
// same guard-removal and guard-ordering regressions.
// Scenario: a user runs `braid tui </dev/null >/dev/null`; we want a clean
// error and exit, not a panic or hang.
#[test]
fn tui_rejects_non_tty_stdio() {
    maybe_run_probe();
    run_child_probe("tui", "tui_rejects_non_tty_stdio");
}

// Intent: braid_cli::tui::run_demo rejects redirected stdio with an io::Error
// before ratatui::init() runs.
// Why it exists: run_demo skips the root gate in main.rs, so it needs its own
// call-site coverage for the TTY guard.
// Scenario: a user runs `braid tui --demo </dev/null >/dev/null`; we want a
// clean error and exit.
#[test]
fn tui_demo_rejects_non_tty_stdio() {
    maybe_run_probe();
    run_child_probe("tui_demo", "tui_demo_rejects_non_tty_stdio");
}
