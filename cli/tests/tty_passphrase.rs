#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

const PROMPT: &str = "LUKS passphrase: ";

fn maybe_run_probe() {
    let Ok(probe) = std::env::var("BRAID_TTY_PROBE") else {
        return;
    };
    match probe.as_str() {
        "test1" => run_probe(probe_pty_integration),
        "test2" => run_probe(probe_deadlock_immunity),
        other => panic!("unknown BRAID_TTY_PROBE value: {other}"),
    }
}

fn run_probe(probe: fn() -> Result<(), String>) -> ! {
    let status = match std::panic::catch_unwind(probe) {
        Ok(Ok(())) => 0,
        Ok(Err(err)) => {
            eprintln!("{err}");
            2
        }
        Err(_) => 1,
    };
    std::process::exit(status);
}

fn run_child_probe(probe: &str, parent_test_name: &str, timeout_message: &str) {
    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .env("BRAID_TTY_PROBE", probe)
        .arg("--exact")
        .arg(parent_test_name)
        .arg("--test-threads=1")
        .spawn()
        .expect("failed to spawn test probe");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("failed to wait for test probe") {
            assert!(status.success(), "probe {probe} exited with {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{timeout_message}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open_pty_pair() -> (File, File) {
    let r = nix::pty::openpty(None, None).expect("openpty failed");
    (File::from(r.master), File::from(r.slave))
}

fn tcgetattr(fd: &File) -> Result<nix::sys::termios::Termios, String> {
    nix::sys::termios::tcgetattr(fd).map_err(|e| format!("tcgetattr failed: {e}"))
}

fn assert_termios_public_eq(
    before: &nix::sys::termios::Termios,
    after: &nix::sys::termios::Termios,
) {
    assert_eq!(before.input_flags, after.input_flags, "input_flags");
    assert_eq!(before.output_flags, after.output_flags, "output_flags");
    assert_eq!(before.control_flags, after.control_flags, "control_flags");
    assert_eq!(before.local_flags, after.local_flags, "local_flags");
    assert_eq!(before.control_chars, after.control_chars, "control_chars");
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "haiku"))]
    assert_eq!(
        before.line_discipline, after.line_discipline,
        "line_discipline"
    );
}

fn probe_pty_integration() -> Result<(), String> {
    let (mut master_file, slave_file) = open_pty_pair();
    let before = tcgetattr(&slave_file)?;

    let got = std::thread::scope(|scope| {
        let reader = scope.spawn(|| braid_cli::luks::read_tty_from_file(&slave_file, PROMPT));

        let mut prompt = vec![0; PROMPT.len()];
        master_file
            .read_exact(&mut prompt)
            .map_err(|e| format!("failed to read prompt from pty master: {e}"))?;
        assert_eq!(prompt, PROMPT.as_bytes());

        master_file
            .write_all(b"hunter2\n")
            .map_err(|e| format!("failed to write passphrase to pty master: {e}"))?;
        reader
            .join()
            .expect("reader thread panicked")
            .map_err(|e| e.to_string())
    })?;
    assert_eq!(got.expose_secret(), "hunter2");

    let after = tcgetattr(&slave_file)?;
    assert_termios_public_eq(&before, &after);
    Ok(())
}

fn probe_deadlock_immunity() -> Result<(), String> {
    let _stdin_guard = std::io::stdin().lock();
    let (mut master_file, slave_file) = open_pty_pair();
    master_file
        .write_all(b"x\n")
        .map_err(|e| format!("failed to write passphrase to pty master: {e}"))?;
    let got =
        braid_cli::luks::read_tty_from_file(&slave_file, PROMPT).map_err(|e| e.to_string())?;
    assert_eq!(got.expose_secret(), "x");
    Ok(())
}

/*
 * Intent: read_tty_from_file writes its prompt to the tty, reads a hidden
 *   passphrase from the same tty, and restores termios afterward.
 * Why it exists: the replacement for rpassword owns echo suppression now, so
 *   the pty-level behavior needs direct coverage.
 * Scenario: braid prompts on an interactive terminal, the user types a LUKS
 *   passphrase, and the terminal returns to its original mode after the read.
 */
#[test]
fn pty_integration() {
    maybe_run_probe();
    run_child_probe(
        "test1",
        "pty_integration",
        "RealTty pty integration probe wedged -- check prompt-write or thread-join in read_tty_from_file",
    );
}

/*
 * Intent: read_tty_from_file does not touch std::io::stdin() while reading a
 *   passphrase from the tty file descriptor.
 * Why it exists: rpassword 5 deadlocked by re-locking stdin while braid
 *   already held the global stdin lock.
 * Scenario: braid has an outer stdin lock in the same thread and still reads
 *   the interactive LUKS passphrase from /dev/tty without wedging.
 */
#[test]
fn deadlock_immunity() {
    maybe_run_probe();
    run_child_probe(
        "test2",
        "deadlock_immunity",
        "read_tty_from_file appears to deadlock -- the rpassword-style same-thread re-lock has been reintroduced",
    );
}
