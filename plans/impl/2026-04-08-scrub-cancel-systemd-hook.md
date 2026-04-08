# Replace `grep finished` ExecStop hook with `braid scrub-cancel --mount`

## Context

`modules/braid/storage.nix:71` ends `braid-scrub.service` with a brittle shell pipeline:

```
(${btrfsProgs}/bin/btrfs scrub status ${cfg.mountPoint} | ${pkgs.gnugrep}/bin/grep finished) || ${btrfsProgs}/bin/btrfs scrub cancel ${cfg.mountPoint}
```

Two problems:

1. **Bypasses the parser canary.** braid already has `parse_btrfs_scrub_status` (`cli/src/parse/btrfs_scrub_status.rs`) with golden fixtures and CLI canary coverage via `braid idle`. Re-implementing the state check via `grep` against btrfs-progs output is exactly what the parser exists to absorb. Per `reference/btrfs-progs/cmds/scrub.c:341-343`, the documented status strings are `running` / `aborted` / `finished` / `interrupted` — `grep finished` matches only one and is unanchored.

2. **ExecStop fails for `Never` / `Completed-but-unmatched` / `aborted` / `interrupted` states.** When `grep finished` returns 1, the `||` fires `btrfs scrub cancel`, which exits non-zero with `ERROR: scrub cancel failed on <mp>: not running` (`scrub.c:1798`). The script's last command is the failed cancel → ExecStop fails → service shows `failed`. The existing `tests/module/scrub-lifecycle.py` cancel node actually exercises this path (the fake ExecStart never runs `btrfs scrub start`), but no assertion checks `Result=success` post-stop, so the bug is silent today.

The fix is to push the state-check into a typed Rust handler that uses the existing parser, and call it from the systemd hook.

## Goal

Replace the inline grep with a new CLI subcommand that:

- Calls `parse_btrfs_scrub_status` and only invokes `btrfs scrub cancel` when state is `Running`.
- Treats `Never` and `Completed` as silent no-op success (exit 0).
- Treats `Unknown` (parser couldn't classify the output) as a **hard error**, not a no-op. `Unknown` is the parser's "I don't know" bucket, not evidence of no-op safety; silently succeeding would hide parser drift while leaving the busy-mount problem unsolved.
- Tolerates the `status==Running` → `cancel says "not running"` race (scrub completed between probe and cancel) as success.
- Exits non-zero only on genuine cancel failure or `Unknown` state.

## Approach

Model the new command on `cli/src/idle.rs` — the simplest existing handler that takes a runner + mount point and returns a typed result. Reuse `CmdRequest::BtrfsScrubStatus` (already exists) and add a new `CmdRequest::BtrfsScrubCancel` variant.

**Mount point comes from an explicit `--mount <path>` arg, not config.** The systemd unit already has `${cfg.mountPoint}` at unit-generation time, so introducing a config-file read into the `ExecStop` shutdown path would be a regression — it adds an unnecessary failure surface (`/etc/braid/config.json` missing/corrupt would fail ExecStop) and violates the thin-systemd-layer principle in [`docs/decisions/018-systemd-lifecycle.md:9`](../../docs/decisions/018-systemd-lifecycle.md). The hook passes the mount directly.

## Detailed changes

### 1. `cli/src/cmd.rs` — add `BtrfsScrubCancel` variant

Add to the `CmdRequest` enum near `BtrfsScrubStatus` (around line 47):

```rust
BtrfsScrubCancel {
    mount_point: MountPoint,
},
```

Add the `to_argv()` arm next to `BtrfsScrubStatus` (around line 324):

```rust
CmdRequest::BtrfsScrubCancel { mount_point } => CmdArgs {
    program: "btrfs",
    args: vec!["scrub".into(), "cancel".into(), mount_point.0.clone()],
},
```

### 2. `cli/src/scrub_cancel.rs` — new handler module

New file. Skeleton (model on `cli/src/idle.rs:49-92`):

```rust
use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::parse::{parse_btrfs_scrub_status, ParseError, ScrubState};
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum ScrubCancelResult {
    /// Scrub was running and we cancelled it.
    Cancelled,
    /// Status said Running but cancel raced with completion ("not running"). Benign.
    RacedCompletion,
    /// No scrub running (Never / Completed). Nothing to do.
    NotRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubCancelError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("btrfs scrub cancel failed: {stderr}")]
    CancelFailed { stderr: String },
    #[error(
        "btrfs scrub status returned an unclassifiable result; refusing to silently no-op a \
         shutdown-path cancel. Investigate parser drift or partial output."
    )]
    StatusUnknown,
}

pub fn cmd_scrub_cancel<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubCancelResult, ScrubCancelError> {
    let status_raw = runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    })?;
    let status = parse_btrfs_scrub_status(&status_raw)?;

    match status.state {
        ScrubState::Running { .. } => {
            let cancel_raw = runner.run(&CmdRequest::BtrfsScrubCancel {
                mount_point: mount_point.clone(),
            })?;
            if cancel_raw.exit_status == 0 {
                Ok(ScrubCancelResult::Cancelled)
            } else if cancel_raw.stderr.contains("not running") {
                // Race: scrub completed between probe and cancel. Treat as success.
                Ok(ScrubCancelResult::RacedCompletion)
            } else {
                Err(ScrubCancelError::CancelFailed {
                    stderr: cancel_raw.stderr,
                })
            }
        }
        ScrubState::Never | ScrubState::Completed { .. } => {
            Ok(ScrubCancelResult::NotRunning)
        }
        ScrubState::Unknown => {
            // Unknown is the parser's "couldn't classify" bucket — NOT evidence
            // that no scrub is running. Failing loud here surfaces parser drift
            // instead of letting it silently break the cancel path.
            Err(ScrubCancelError::StatusUnknown)
        }
    }
}
```

Plus a `#[cfg(test)] mod tests` block — see the test section below.

### 3. `cli/src/lib.rs` — export the new module

Add `pub mod scrub_cancel;` (alphabetic position around line 30, after `state_io` or near `status`).

### 4. `cli/src/main.rs` — wire the clap subcommand

Add an args struct (model on existing single-arg structs around lines 76-82):

```rust
#[derive(Debug, Args)]
struct ScrubCancelArgs {
    /// Mount point of the braid pool to check
    #[arg(long)]
    mount: String,
}
```

Add to `enum Commands` (around line 45, near `Idle`):

```rust
/// Cancel a btrfs scrub on the given mount.
///
/// Probes scrub state via `btrfs scrub status` first; only invokes
/// `btrfs scrub cancel` when a scrub is actually running. No-op (exit 0)
/// when no scrub is running. Hard-fails if the parser cannot classify
/// the status output (Unknown), so parser drift surfaces instead of
/// silently masking a busy mount. Internal: invoked by
/// braid-scrub.service ExecStop.
ScrubCancel(ScrubCancelArgs),
```

Add the dispatch arm. Note: this command does **not** read config — the mount comes from `--mount` so the systemd shutdown path has zero filesystem dependencies beyond the binary itself.

```rust
Commands::ScrubCancel(args) => {
    let runner = RealRunner;
    let mount_point = braid_cli::types::MountPoint(args.mount.clone());
    match braid_cli::scrub_cancel::cmd_scrub_cancel(&runner, &mount_point) {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    }
}
```

### 5. `modules/braid/storage.nix:66-72` — replace the grep hook

```nix
ExecStop = pkgs.writeShellScript "braid-scrub-maybe-cancel" ''
  # If pool is already unmounted during shutdown race, nothing remains to cancel.
  ${utilLinux}/bin/mountpoint -q ${cfg.mountPoint} || exit 0

  # Use the typed scrub-status parser instead of grep — see
  # cli/src/scrub_cancel.rs. Only cancels if status is Running; clean no-op for
  # Never/Completed; hard-fails on Unknown so parser drift surfaces instead of
  # silently masking a busy mount. Mount is passed explicitly — ExecStop has
  # no config-file dependency.
  exec ${braidWrapped}/bin/braid scrub-cancel --mount ${cfg.mountPoint}
'';
```

`braidWrapped` is already in the file's `let` binding and is the same path used by `braid-online.service`'s ExecStop (line 92), so no new imports.

## Tests

### Unit tests — `cli/src/scrub_cancel.rs`

Use `MockRunner` per `cli/src/idle.rs:115-449`. Borrow the helper functions from `idle.rs` (`scrub_running`, `scrub_completed`) — but inline copies in this module are fine; we don't need to extract a shared helper.

Required cases (each with a doc block per AGENTS.md test conventions):

1. `running_invokes_cancel` — status `running` → `BtrfsScrubCancel` mocked to exit 0 → `Cancelled`. **Failure-layer test:** if a future refactor stops calling `BtrfsScrubCancel` when state is Running, MockRunner panics with MissingMock (or assertion fails).
2. `never_does_not_invoke_cancel` — status `no stats available` → no cancel mock seeded → returns `NotRunning` (would panic if cancel were invoked).
3. `completed_does_not_invoke_cancel` — same shape; `Status: finished` → `NotRunning`.
4. `unknown_is_hard_error` — empty stdout (parser returns `ScrubState::Unknown`) → returns `Err(StatusUnknown)`. **This is the failure-layer guard against silently masking parser drift.** No cancel mock seeded; would panic if cancel were attempted.
5. `cancel_race_with_completion_is_success` — status `running` → cancel mock exits 1 with stderr `"ERROR: scrub cancel failed on /mnt/storage: not running"` → returns `RacedCompletion`.
6. `cancel_real_failure_propagates` — status `running` → cancel mock exits 1 with stderr `"ERROR: permission denied"` → returns `Err(CancelFailed)`.
7. `status_command_failure_propagates` — status mock with `exit_status: 1` → returns `Err(Parse(CommandFailed { .. }))`. (Matches `idle.rs`'s `replace_status_failure_is_not_idle` shape.)

### VM test — extend `tests/module/scrub-lifecycle.py` cancel node

The existing cancel node already exercises the bug path silently. Extend it (don't create a new test file — reuse the proven pattern per the established VM-test conventions):

After the existing `cancel.succeed("braid lock")` block (line 112-115), add:

```python
with subtest("cancel: ExecStop succeeded (no false-fail in Never state)"):
    # The fake scrub never ran `btrfs scrub start`, so scrub state is `Never`.
    # The old shell hook would call `btrfs scrub cancel`, which exits non-zero
    # with "not running", marking the service as `failed`. This subtest is the
    # failure-layer guard for that bug.
    result = show(cancel, SERVICE, "Result")
    assert result == "success", f"braid-scrub.service ExecStop failed: Result={result}"
```

This subtest **fails on the old hook** (Result would be `exit-code` because `btrfs scrub cancel` exits 1 in the `Never` state), and **passes on the new hook** (because `cmd_scrub_cancel` matches `ScrubState::Never` and exits 0 cleanly).

Add a second subtest covering the real Running → cancel path. Use the existing `catchup` node infrastructure or add a third node — but keeping it in `cancel` is simpler: replace the fake `sleep 300` with a real long-running scrub on a slightly larger disk, then assert (a) cancel was actually invoked (e.g., `btrfs scrub status` after stop reports `aborted`), and (b) `Result=success`.

**Decision: keep VM coverage minimal.** The seven unit tests above cover all four `ScrubState` branches plus race/error paths against MockRunner. The VM test only needs to prove the wiring works end-to-end and that the `Never` false-fail is fixed. So just the one extension to the existing `cancel` node is sufficient — do not add a third VM node.

### Parser canary

No changes needed. `parse_btrfs_scrub_status` is already in the CLI canary lane via `braid idle` (`tests/cli/braid-idle.py:43-60`). The new subcommand calls the same parser, so the canary continues to protect both call sites. No need to add `braid scrub-cancel` to the `test-parsers` recipe.

## Critical files

| File | Change |
|------|--------|
| `cli/src/cmd.rs:47` (enum) and `cli/src/cmd.rs:316` (to_argv) | Add `BtrfsScrubCancel` variant + arm |
| `cli/src/scrub_cancel.rs` | **New file** — handler + unit tests |
| `cli/src/lib.rs:30` | Add `pub mod scrub_cancel;` |
| `cli/src/main.rs:45` (enum), `cli/src/main.rs:476` (dispatch) | Add `ScrubCancel` subcommand + dispatch arm |
| `modules/braid/storage.nix:66-72` | Replace grep pipeline with `braid scrub-cancel` |
| `tests/module/scrub-lifecycle.py:113` (after cancel block) | Add `Result=success` assertion |

## Verification

1. **Rust unit tests:** `just test-rust` — must pass, all seven `scrub_cancel` cases.
2. **VM test (focused):** `just test-vm scrub-lifecycle` — must pass; the new `Result=success` assertion is the failure-layer guard for this fix.
3. **Bug-reintroduction sanity check:** temporarily revert `modules/braid/storage.nix:66-72` to the old grep pipeline and rerun `just test-vm scrub-lifecycle`. The new assertion **must fail**. Restore the fix afterward. (This is the test-at-failure-layer protocol.)
4. **Parser canary:** `just test-parsers` — must continue to pass; no parser code changed but the canary confirms `parse_btrfs_scrub_status` is still wired through `braid idle`.
5. **Build sanity:** `just test-vm` (full default suite) — confirms no NixOS module regressions from the storage.nix edit.

No `nixpkgs` bump and no parser-critical tool change, so fixture refresh (`just capture-all-fixtures`) is **not** required.

## Out of scope

- No config-file read in the new subcommand — mount comes from `--mount`. Keeps ExecStop free of `/etc/braid/config.json` failure modes and aligns with the thin-systemd-layer principle.
- No findmnt probe inside the Rust handler — the systemd hook keeps the existing `mountpoint -q` shell guard for the shutdown-race case. Cleaner separation, fewer subprocess calls.
- No new TUI, browse, or status integration — `cmd_scrub_cancel` is a single-purpose systemd-callable command.
- No changes to the parser, the fixtures, or `just test-parsers` recipe.
- No removal of the existing `tests/module/scrub-lifecycle.py` cancel node — extend, don't replace.
