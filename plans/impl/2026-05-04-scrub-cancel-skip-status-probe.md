# Plan: skip the status probe; cancel directly via the kernel ioctl

## Context

`cmd_scrub_cancel` (cli/src/scrub_cancel.rs) currently runs `btrfs scrub
status` first and only invokes `btrfs scrub cancel` when the parser
classifies the state as `Running`. This forces a userspace round-trip
between the shutdown path and the kernel's authoritative answer, and
that round-trip has three independent failure modes that all leave a
live kernel scrub holding the mount through `braid lock`:

1. **Status command fails.** Any `BtrfsScrubStatus` error
   (transient EIO, partial mount degradation, stale parser, etc.)
   propagates as `ScrubCancelError::Parse`. ExecStop exits 1.
2. **Parser cannot classify (`Unknown`).** Upstream output format drift
   on a nixpkgs bump returns `Err(StatusUnknown)`. ExecStop exits 1.
3. **Userspace status lies (`Never` while kernel scrubbing).** A
   freshly started scrub whose foreground process dies before the first
   checkpoint write produces `\tno stats available` (scrub.c:313-323
   gates on `t_start != 0`); the parser maps that to
   `ScrubState::Never` and silent-no-ops the cancel. ExecStop exits 0
   but no cancel ioctl was issued.

In every case, systemd then SIGTERMs the foreground `btrfs scrub start
-B`, which only handles SIGINT (scrub.c:436-444). Default SIGTERM
termination skips `BTRFS_IOC_SCRUB_CANCEL`. The kernel scrub keeps
running. `braid lock` fails EBUSY.

The cancel ioctl is itself the kernel-authoritative test: btrfs reports
`ENOTCONN -> exit 2 + "not running" stderr` when no scrub is active
(scrub.c:1796-1803), and exit 0 with `"scrub cancelled\n"` when one
was. That single command answers everything the shutdown path needs to
know.

Outcome: drop the status probe entirely from the cancel path. Issue
the cancel ioctl directly and classify on its result. The shutdown path
becomes immune to status command failure, parser drift, and
userspace/kernel state divergence -- the three failure modes converge
into "the cancel ioctl handles it." `parse_btrfs_scrub_status` stays;
it is still used by `scrub_needs_resume.rs`, `status.rs`, and the TUI
(`browse/model.rs`), where parser drift surfaces as a non-shutdown-
breaking error.

## Files

- `cli/src/scrub_cancel.rs` -- primary change; rewrite around cancel
  only. Drop `parse_btrfs_scrub_status` import and
  `BtrfsScrubStatus` mock plumbing from this file.
- `modules/braid/storage.nix:13-30` -- update the
  `scrubCancelScript` comment to reflect the new contract (kernel
  ioctl, no parser dependency, idle = success). No code change to the
  script itself; the existing post-success `sleep 2` still gives the
  foreground process time to checkpoint when a real cancel happened.
- `cli/src/main.rs:585` -- single production caller; pattern still
  works (`Ok(_) => exit(0)`). No change.

Untouched: `cli/src/parse/btrfs_scrub_status.rs`,
`cli/src/scrub_needs_resume.rs`, `cli/src/status.rs`,
`cli/src/browse/model.rs`. The status parser remains the right tool
for resume detection and TUI display, where the failure modes above
do not break shutdown.

## Approach

```rust
pub enum ScrubCancelResult {
    Cancelled,   // cancel ioctl exit 0 -- kernel scrub was running
    NotRunning,  // cancel ioctl ENOTCONN -- idle (benign)
}

pub enum ScrubCancelError {
    Cmd(CmdError),
    CancelFailed { stderr: String },
}

pub fn cmd_scrub_cancel<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubCancelResult, ScrubCancelError> {
    let raw = runner.run(&CmdRequest::BtrfsScrubCancel {
        mount_point: mount_point.clone(),
    })?;

    if raw.exit_status == 0 {
        Ok(ScrubCancelResult::Cancelled)
    } else if raw.stderr.contains("not running") {
        // ENOTCONN, mapped to exit 2 by btrfs-progs (scrub.c:1799-1800).
        // We match on stderr rather than exit code 2 to stay aligned with
        // the existing convention and tolerate future exit-code adjustments.
        Ok(ScrubCancelResult::NotRunning)
    } else {
        Err(ScrubCancelError::CancelFailed { stderr: raw.stderr })
    }
}
```

**Removed surface:**

- `ScrubCancelResult::RacedCompletion` -- was a status-vs-cancel
  diagnostic; no caller distinguishes it from `NotRunning`.
- `ScrubCancelError::StatusUnknown` -- now unreachable.
- `ScrubCancelError::Parse` -- no parser invoked.
- The `parse_btrfs_scrub_status` import and all `scrub_status_*`
  fixture helpers from this file (lines 91-194).

This is acceptable per AGENTS.md "No backwards compatibility": braid is
unreleased, no migration shims.

## storage.nix comment update

Replace lines 17-21 of `modules/braid/storage.nix` with text describing
the new contract:

```nix
# braid scrub-cancel calls the kernel BTRFS_IOC_SCRUB_CANCEL ioctl
# directly. It is the kernel-authoritative path -- no userspace status
# round-trip, no parser dependency. An idle filesystem returns
# ENOTCONN (mapped to "not running" stderr) and exits 0 from braid;
# only real cancel-ioctl errors propagate. Mount is passed explicitly
# -- ExecStop has no config-file dependency.
```

The existing `mountpoint -q` short-circuit and the post-success `sleep
2` (to let the foreground `btrfs scrub start -B` rewrite
`scrub.status.<fsid>` with `canceled=1` before SIGTERM) stay as-is.

## Tests

All in `cli/src/scrub_cancel.rs`. Replace the entire `mod tests`
block. Down from nine tests to four.

**New test set:**

1. `cancel_running_returns_cancelled` -- mock `BtrfsScrubCancel`
   returning exit 0, stdout `"scrub cancelled\n"`, empty stderr.
   Assert `Ok(Cancelled)`.

2. `cancel_idle_returns_not_running` -- mock exit 2, empty stdout,
   stderr `"ERROR: scrub cancel failed on /mnt/storage: not
   running\n"`. Assert `Ok(NotRunning)`. (Exit code 2, not 1; the
   existing `scrub_cancel_not_running` fixture is wrong and gets
   corrected here.)

3. `cancel_real_failure_propagates` -- mock exit 1, stderr `"ERROR:
   permission denied\n"`. Assert `Err(CancelFailed)`.

4. `cancel_command_failure_propagates` -- runner returns
   `CmdError`. Assert `Err(Cmd)`.

Each test gets the standard three-section block comment (Intent / Why
it exists / Scenario), per AGENTS.md "Test Conventions". The "Why it
exists" sections name the structural property each test pins:
kernel-authoritative cancel, idle benignness, real-error propagation,
command-layer error propagation.

**Deleted tests:**

`running_invokes_cancel`, `never_does_not_invoke_cancel`,
`finished_does_not_invoke_cancel`, `aborted_does_not_invoke_cancel`,
`interrupted_does_not_invoke_cancel`, `unknown_is_hard_error`,
`cancel_race_with_completion_is_success`,
`status_command_failure_propagates`. All asserted properties of the
status-probe path that no longer exists.

**Deleted helpers:**

`scrub_status_running`, `scrub_status_never`,
`scrub_status_finished`, `scrub_status_aborted`,
`scrub_status_interrupted`, `scrub_status_unknown`. Unused after the
test rewrite.

## Verification

1. **Unit tests:**
   `just test-rust` -- exercises the four-branch surface.

2. **VM test:**
   `just test-vm scrub-lifecycle` -- two subtests in
   `tests/module/scrub-lifecycle.py` lock the new behavior end-to-end:
   - `cancel: ExecStop succeeded (no false-fail in Never state)`
     (line 217) -- fake-scrub node, scrub state is `Never`, the cancel
     ioctl returns ENOTCONN with `"not running"` stderr. Pins the
     idle/`not running` mapping against live btrfs-progs output, so
     parser-free dependence on that wording is grounded in real-tool
     behavior on every test run.
   - `resume: cancel preserves Aborted state across lock/unlock`
     (line 243) -- dm-delay-backed real scrub, cancel mid-scrub. Pins
     that `BTRFS_IOC_SCRUB_CANCEL` actually fires through ExecStop and
     the kernel transitions the scrub to the `aborted` (canceled=1)
     state recorded in `scrub.status.<fsid>`.

   No test addition needed; both subtests already exist and exercise
   the path the rewritten `cmd_scrub_cancel` takes.

3. **Parser-compat lanes:** unaffected. The parser is unchanged and
   parser drift no longer reaches the shutdown path. `just
   test-parsers` is informational only.
