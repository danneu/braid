#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn maybe_run_probe() {
    if std::env::var("BRAID_CONFIRM_YES_PROBE").is_err() {
        return;
    }

    if let Err(e) = braid_cli::confirm::confirm_yes() {
        eprintln!("confirm_yes failed: {e}");
        std::process::exit(2);
    }

    let stdin_fd = match nix::unistd::dup(std::io::stdin()) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("dup(stdin) after confirm failed: {e}");
            std::process::exit(3);
        }
    };
    let mut tail = std::fs::File::from(stdin_fd);
    let mut buf = Vec::new();
    if let Err(e) = tail.read_to_end(&mut buf) {
        eprintln!("read remaining stdin failed: {e}");
        std::process::exit(4);
    }
    if buf != b"secret\n" {
        eprintln!(
            "post-confirm stdin tail mismatch: expected b\"secret\\n\", got {:?}",
            buf
        );
        std::process::exit(5);
    }

    std::process::exit(0);
}

// Intent: confirm_yes consumes only the confirmation line from real process stdin.
// Why it exists: confirm_yes now duplicates fd 0 before wrapping it in File,
//   and must preserve fd 0 plus any bytes intended for the next stdin reader.
// Scenario: a destructive command asks for "yes" before reading a piped
//   passphrase from the same stdin stream.
#[test]
fn confirm_yes_does_not_predrain_following_bytes() {
    maybe_run_probe();

    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .env("BRAID_CONFIRM_YES_PROBE", "1")
        .arg("--exact")
        .arg("confirm_yes_does_not_predrain_following_bytes")
        .arg("--test-threads=1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn confirm_yes probe");

    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        stdin
            .write_all(b"yes\nsecret\n")
            .expect("write to child stdin");
    }
    drop(child.stdin.take());

    let status = child.wait().expect("wait for child");
    assert!(
        status.success(),
        "confirm_yes() must accept yes\\n and leave following bytes \
         readable from fd 0 -- child exited {status:?}. Exit codes: \
         2=confirm failed, 3=post-confirm dup failed, 4=read failed, \
         5=tail bytes mismatch."
    );
}
