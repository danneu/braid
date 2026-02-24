use std::process::Command;

fn is_root() -> bool {
    // SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
    (unsafe { libc::geteuid() }) == 0
}

fn braid() -> Command {
    Command::new(env!("CARGO_BIN_EXE_braid"))
}

#[test]
fn non_root_exits_with_error() {
    if is_root() {
        return;
    }
    let output = braid().arg("status").output().expect("failed to execute braid");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must be run as root"), "got: {stderr}");
}

#[test]
fn help_works_without_root() {
    if is_root() {
        return;
    }
    let output = braid().arg("--help").output().expect("failed to execute braid");
    assert!(output.status.success(), "expected success, got {:?}", output.status);
}

#[test]
fn version_works_without_root() {
    if is_root() {
        return;
    }
    let output = braid().arg("--version").output().expect("failed to execute braid");
    assert!(output.status.success(), "expected success, got {:?}", output.status);
}

#[test]
fn apply_json_flag_accepted() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["apply", "--help"])
        .output()
        .expect("failed to execute braid");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--json"), "apply --help should show --json, got: {stdout}");
}

#[test]
fn apply_progress_values_accepted() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["apply", "--help"])
        .output()
        .expect("failed to execute braid");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--progress"), "apply --help should show --progress, got: {stdout}");
    for val in ["auto", "always", "never"] {
        assert!(stdout.contains(val), "apply --help should show {val}, got: {stdout}");
    }
}
