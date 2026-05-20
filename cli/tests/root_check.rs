use std::process::Command;

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn braid() -> Command {
    Command::new(env!("CARGO_BIN_EXE_braid"))
}

#[test]
fn non_root_exits_with_error() {
    if is_root() {
        return;
    }
    let output = braid()
        .arg("status")
        .output()
        .expect("failed to execute braid");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must be run as root"), "got: {stderr}");
}

// Intent: `braid doctor --beep` is rejected as non-root by the universal root
//   gate in main.rs, never reaching cmd_doctor or the beep-check logic.
// Why it exists: check_beep_path / check_beep_path_inner no longer carry a
//   defense-in-depth root skip arm; that branch was deleted because main.rs
//   already gates every command except `tui --demo` and `help`.
// Scenario: an unprivileged user runs `braid doctor --beep` to preview the
//   alert beep without sudo.
#[test]
fn non_root_doctor_exits_with_error() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["doctor", "--beep"])
        .output()
        .expect("failed to execute braid");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must be run as root"), "got: {stderr}");
}

#[test]
fn help_works_without_root() {
    if is_root() {
        return;
    }
    let output = braid()
        .arg("--help")
        .output()
        .expect("failed to execute braid");
    assert!(
        output.status.success(),
        "expected success, got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("(s)"),
        "help should not contain literal plural marker, got: {stdout}"
    );
}

// Intent: `braid help <command>` remains a non-root help path.
// Why it exists: braid replaces Clap's generated help subcommand text so root
//   help output can avoid literal `(s)` plural markers.
// Scenario: a user runs `braid help add` before sudo and sees the same add
//   command help they would get from `braid add --help`.
#[test]
fn help_subcommand_works_without_root() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["help", "add"])
        .output()
        .expect("failed to execute braid");
    assert!(
        output.status.success(),
        "expected success, got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage: braid add"),
        "help add should show the full add command usage, got: {stdout}"
    );
    assert!(
        stdout.contains("--config"),
        "help add should include global options, got: {stdout}"
    );
    assert!(
        stdout.contains("--dry-run"),
        "help add should show --dry-run, got: {stdout}"
    );
    assert!(
        !stdout.contains("(s)"),
        "help add should not contain literal plural marker, got: {stdout}"
    );
}

#[test]
fn version_works_without_root() {
    if is_root() {
        return;
    }
    let output = braid()
        .arg("--version")
        .output()
        .expect("failed to execute braid");
    assert!(
        output.status.success(),
        "expected success, got {:?}",
        output.status
    );
}

#[test]
fn add_dry_run_flag_accepted() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["add", "--help"])
        .output()
        .expect("failed to execute braid");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--dry-run"),
        "add --help should show --dry-run, got: {stdout}"
    );
    assert!(
        stdout.contains("--yes"),
        "add --help should show --yes, got: {stdout}"
    );
    assert!(
        !stdout.contains("(s)"),
        "add --help should not contain literal plural marker, got: {stdout}"
    );
}

// Intent: `braid add` with no disk arguments must be rejected by clap
//   before any application logic runs.
// Why: Without this, an empty disk list flows into cmd_add, hits the
//   steps.is_empty() branch with an empty label, and exits 0 with a
//   grammatically broken "Nothing to do" message instead of a usage error.
// Scenario: User runs `sudo braid add` (forgetting the disk spec).
//   Expected: clap prints a usage error and exits 2.
//   Actual (before fix): exits 0 with misleading "already in pool" message.
#[test]
fn add_requires_at_least_one_disk() {
    let output = braid()
        .arg("add")
        .output()
        .expect("failed to execute braid");
    // Clap exits 2 for usage errors; braid's own errors exit 1.
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("required"),
        "expected clap usage error mentioning required args, got: {stderr}"
    );
}

#[test]
fn add_progress_values_accepted() {
    if is_root() {
        return;
    }
    let output = braid()
        .args(["add", "--help"])
        .output()
        .expect("failed to execute braid");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--progress"),
        "add --help should show --progress, got: {stdout}"
    );
    for val in ["auto", "always", "never"] {
        assert!(
            stdout.contains(val),
            "add --help should show {val}, got: {stdout}"
        );
    }
}
