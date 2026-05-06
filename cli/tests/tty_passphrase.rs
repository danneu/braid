#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
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
    let mut master = -1;
    let mut slave = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

fn tcgetattr(fd: RawFd) -> Result<libc::termios, String> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::zeroed();
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc == -1 {
        Err(format!(
            "tcgetattr failed: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(unsafe { termios.assume_init() })
    }
}

fn termios_bytes(termios: &libc::termios) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            (termios as *const libc::termios).cast::<u8>(),
            std::mem::size_of::<libc::termios>(),
        )
    }
}

fn assert_termios_eq(expected: &libc::termios, actual: &libc::termios) {
    assert_eq!(termios_bytes(expected), termios_bytes(actual));
}

fn probe_pty_integration() -> Result<(), String> {
    let (mut master_file, mut slave_file) = open_pty_pair();
    let before = tcgetattr(slave_file.as_raw_fd())?;

    let got = std::thread::scope(|scope| {
        let reader = scope.spawn(|| braid_cli::luks::read_tty_from_file(&mut slave_file, PROMPT));

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
    assert_eq!(got.as_str(), "hunter2");

    let after = tcgetattr(slave_file.as_raw_fd())?;
    assert_termios_eq(&before, &after);
    Ok(())
}

fn probe_deadlock_immunity() -> Result<(), String> {
    let _stdin_guard = std::io::stdin().lock();
    let (mut master_file, mut slave_file) = open_pty_pair();
    master_file
        .write_all(b"x\n")
        .map_err(|e| format!("failed to write passphrase to pty master: {e}"))?;
    let got =
        braid_cli::luks::read_tty_from_file(&mut slave_file, PROMPT).map_err(|e| e.to_string())?;
    assert_eq!(got.as_str(), "x");
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
