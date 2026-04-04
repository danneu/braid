# Fix: report signal-killed child processes instead of returning exit -1

## Context

When a subprocess is killed by a signal (SIGKILL from OOM, SIGPIPE, etc.), `ExitStatus::code()` returns `None` on Unix — the process never exited normally. `cmd.rs` collapses this to `-1` via `unwrap_or(-1)`, which flows into 50+ error messages as a confusing `"failed (exit -1)"` with likely empty stderr. The user has no idea the process was signal-killed.

The `-1` sentinel also creates a false positive in the smartctl parser (`-1 & 0x07 == 7` looks like a command-line error).

## Approach

Return `Err(CmdError::Failed(...))` immediately when a child process is signal-killed, instead of constructing a `RawCommandOutput` with a fake exit code. This is correct because:
- No downstream caller can meaningfully interpret a signal-killed process's "exit code"
- All callers already propagate `CmdError` via `?` — no call sites need changes
- The error message becomes actionable: `"cryptsetup luksOpen /dev/sda: killed by signal 9 (SIGKILL)"`

Extract the shared `Output` → `RawCommandOutput` conversion into a helper to eliminate duplication between `exec()` and `exec_with_stdin()`.

## Changes — single file: `cli/src/cmd.rs`

### 1. Add import (top of file)
```rust
use std::os::unix::process::ExitStatusExt;
```
No `#[cfg(unix)]` needed — braid is Unix-only and already uses `std::os::unix` in other modules without guards.

### 2. Add `signal_name()` helper (near line 742, before `RealRunner`)

Use `libc::SIG*` constants (already a dependency — used in `main.rs` and `hdparm.rs`) instead of numeric literals, so the mapping is correct on any Unix host (macOS dev machines, Linux VMs).

```rust
fn signal_name(sig: i32) -> &'static str {
    match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        _ => "unknown",
    }
}
```

### 3. Extract `output_to_raw()` helper (after `signal_name`)
```rust
fn output_to_raw(cmd_str: String, output: std::process::Output) -> Result<RawCommandOutput, CmdError> {
    let exit_status = match output.status.code() {
        Some(code) => code,
        None => {
            let sig = output.status.signal().unwrap_or(0);
            let name = signal_name(sig);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                format!("{cmd_str}: killed by signal {sig} ({name})")
            } else {
                format!("{cmd_str}: killed by signal {sig} ({name}): {stderr}")
            };
            return Err(CmdError::Failed(detail));
        }
    };

    Ok(RawCommandOutput {
        cmd: cmd_str,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_status,
    })
}
```

### 4. Simplify `exec()` and `exec_with_stdin()`

Replace the inline `output.status.code().unwrap_or(-1)` + struct construction with a call to `output_to_raw(cmd_str, output)` in both methods.

### 5. Add tests

Three tests, each with required Intent/Why/Scenario block comments.

**Test 1: `output_to_raw` with signal-killed status (deterministic, no subprocess)**

Construct a signaled `ExitStatus` via `ExitStatus::from_raw(libc::SIGKILL)` (raw wait status: signal in low 7 bits, no exit code). This tests `output_to_raw` directly — covers both `exec()` and `exec_with_stdin()` since both delegate to it.

```rust
// Intent: output_to_raw returns CmdError::Failed with signal number and name
//   when the child was killed by a signal.
// Why: Without this, signal kills silently become exit_status=-1, producing
//   confusing "failed (exit -1)" messages with no indication of what happened.
// Scenario: OOM-killer sends SIGKILL to cryptsetup during luksOpen — braid
//   must report the signal, not a mysterious -1.
#[test]
fn output_to_raw_signal_killed_returns_error() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    let status = ExitStatus::from_raw(libc::SIGKILL); // raw=9, no exit code
    let output = std::process::Output {
        status,
        stdout: Vec::new(),
        stderr: b"partial output".to_vec(),
    };
    let result = output_to_raw("cryptsetup luksOpen /dev/sda".into(), output);
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("signal 9"), "expected signal 9 in: {msg}");
    assert!(msg.contains("SIGKILL"), "expected SIGKILL in: {msg}");
    assert!(msg.contains("partial output"), "expected stderr in: {msg}");
}
```

**Test 2: `output_to_raw` with normal exit (deterministic, confirms happy path unchanged)**

```rust
// Intent: output_to_raw returns Ok(RawCommandOutput) for normal exits.
// Why: Refactoring exec()/exec_with_stdin() to use output_to_raw must not
//   change behavior for the normal (non-signal) path.
// Scenario: Any normal command execution — exit 0 or non-zero exit code.
#[test]
fn output_to_raw_normal_exit_returns_output() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    // exit code 5: raw wait status = 5 << 8 = 0x0500
    let status = ExitStatus::from_raw(5 << 8);
    let output = std::process::Output {
        status,
        stdout: b"some stdout".to_vec(),
        stderr: b"some stderr".to_vec(),
    };
    let raw = output_to_raw("test cmd".into(), output).unwrap();
    assert_eq!(raw.exit_status, 5);
    assert_eq!(raw.stdout, "some stdout");
    assert_eq!(raw.stderr, "some stderr");
}
```

**Test 3: `signal_name` mapping**

```rust
// Intent: signal_name returns correct POSIX names for common signals and
//   "unknown" for unrecognized values.
// Why: Wrong signal names in error messages would mislead debugging.
// Scenario: User sees "killed by signal 9 (SIGKILL)" — the name must match.
#[test]
fn signal_name_maps_known_signals() {
    assert_eq!(signal_name(libc::SIGKILL), "SIGKILL");
    assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
    assert_eq!(signal_name(libc::SIGPIPE), "SIGPIPE");
    assert_eq!(signal_name(999), "unknown");
}
```

## What does NOT change
- `RawCommandOutput` struct — unchanged
- `CmdError` enum — no new variant; `Failed(String)` is sufficient
- `MockRunner` — mocks always return concrete exit codes; signal kills are real-process-only
- All 50+ downstream call sites — they already `?`-propagate `CmdError`
- `CommandRunner` trait signature — unchanged

## Verification

1. `just test-rust` — new tests pass, existing unit + golden parser tests unaffected
2. `just test-parsers` — live VM parser canary still passes
