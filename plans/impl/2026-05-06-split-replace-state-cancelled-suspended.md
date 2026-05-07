# Split `ReplaceState::None` into `NotStarted`, `Cancelled`, `Suspended`

## Context

`parse_btrfs_replace_status` (`cli/src/parse/btrfs_replace_status.rs:25-43`) collapses three semantically distinct upstream states -- `BTRFS_IOCTL_DEV_REPLACE_STATE_CANCELED`, `BTRFS_IOCTL_DEV_REPLACE_STATE_SUSPENDED`, and `BTRFS_IOCTL_DEV_REPLACE_STATE_NEVER_STARTED` (per `reference/btrfs-progs/cmds/replace.c:451-505`) -- into a single `ReplaceState::None` variant that is also used for empty stdout, "no operation running", and unrecognised content.

The states are not interchangeable. Per `reference/linux/fs/btrfs/dev-replace.c:1280-1305`, `btrfs_dev_replace_is_ongoing` returns true for both `STARTED` and `SUSPENDED` -- the kernel comment is explicit: *"This does not stop the dev_replace procedure. It needs to be canceled manually if the cancellation is wanted."* And `dev-replace.c:645-656` rejects a fresh `btrfs replace start` with `RESULT_ALREADY_STARTED` if state is `SUSPENDED`. So a `Suspended` state read by recover is recover-blocking: continuing past it and clearing the journal would leave the operator with no braid journal but a kernel dev_replace that blocks retry.

`CANCELED` is different: per `dev-replace.c:1107-1165` and `:1045-1067`, the kernel reverts the pool topology to pre-replace, destroys the tgtdev, and reports `progress_1000 = 0` (the displayed percentage is always `0.0%`). Recovery's existing topology classifier (`execute_replace_pool_mutation_recovery`, `recover.rs:2496-2562`) routes a kernel-canceled replace correctly via `pre_topology = true → finish_uncommitted_replace_recovery`, which preserves the new disk's LUKS state and prints a clear retry message.

The earlier hardening (`plans/impl/2026-04-02-replace-status-exit-code.md`, commit `cc7ba4d`) added an exit-code guard but did not split the variant collapse. No existing plan covers this work; no fixtures or unit tests cover canceled/suspended/never-started output today.

`cli/src/idle.rs` no longer uses this parser (sysfs exclop scan), so consumers are limited to `recover.rs` and `progress.rs`.

## Plan

### 1. Split the enum, asymmetric on percentage

`cli/src/parse/types.rs:393-407`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ReplaceState {
    /// Filesystem has never had a replace issued, OR `btrfs replace status`
    /// returned empty/"no operation running" output. Lenient bucket --
    /// distinguishing "never" from "kernel cleared state" is not actionable.
    NotStarted,
    Running { pct: f64 },
    Finished,
    /// Kernel-canceled. Pool topology has been reverted to pre-replace
    /// (`dev-replace.c:1107-1165`). No percentage carried because the kernel
    /// always reports `progress_1000 = 0` for CANCELED
    /// (`dev-replace.c:1051-1054`).
    Cancelled,
    /// Kernel-suspended replace. The kernel still considers the procedure
    /// ongoing (`dev-replace.c:1280-1305`); a fresh `btrfs replace start` is
    /// rejected with ALREADY_STARTED (`dev-replace.c:645-656`). Operator must
    /// manually cancel before retry. `pct` is the real progress at suspend
    /// time (`dev-replace.c:1058-1063`).
    Suspended { pct: f64 },
}
```

Drop the `BtrfsReplaceStatusOutput` wrapper (`types.rs:404-407`) -- it has only one field. The parser returns `Result<ReplaceState, ParseError>` directly. Two callers (`recover.rs:2719-2731`, `progress.rs:367-369`) update from `parsed.state` to `parsed`.

### 2. Update the parser

`cli/src/parse/btrfs_replace_status.rs:12-43`. Detect prefixes in this order (after the existing exit-code guard):

1. `"finished on"` → `Finished`
2. `"canceled on"` → `Cancelled` (no percent extraction)
3. `"suspended on"` → `Suspended { pct }` (parse percent from `"at NN.N%,"` form; if missing, lenient `0.0`)
4. `"Never started"` → `NotStarted`
5. `"% done"` → `Running { pct }` (existing path)
6. fallback → `NotStarted` (existing lenient behavior; pinned by `garbage_output_treated_as_not_started`)

Generalize `extract_percent` to find an `NN.N%` token anywhere; both `"NN.N% done"` and `"at NN.N%,"` match.

### 3. Rewire the wait loop -- typed Result

`cli/src/recover.rs:2693-2771`. Change the signature:

```rust
fn wait_for_kernel_replace_to_finish<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    sleeper: &dyn Sleeper,
    color_enabled: bool,
) -> Result<(), RecoverError>
```

| State | Behavior |
| --- | --- |
| `Finished` \| `NotStarted` | existing terminal path -- emit `[ok] kernel dev_replace finished` if a wait was previously emitted, return `Ok(())` |
| `Running { pct }` | existing -- emit pct line / heartbeat, sleep, poll |
| `Cancelled` | unconditionally emit `[fail] pool: kernel dev_replace canceled`, return `Ok(())`. Recovery proceeds; downstream `finish_uncommitted_replace_recovery` handles the cleanup and final operator message. |
| `Suspended { pct }` | unconditionally emit `[fail] pool: kernel dev_replace is suspended at NN.N% (target device unavailable). Run \`btrfs replace cancel <MOUNT>\` to clear it, then re-run \`braid recover\`.`, return `Err(RecoverError::Failed(...))` carrying the same text. |
| status command Err / parser Err | existing best-effort -- emit `[warn]` if a wait was emitted, return `Ok(())` |

Update the call site in `RecoverWorkAction::execute` (`recover.rs:433-443`) to propagate via `?`:

```rust
RecoverWorkAction::WaitForKernelReplace => {
    if state.just_mounted {
        wait_for_kernel_replace_to_finish(
            runner, &plan.mount_point, &progress::RealSleeper, color_enabled_for_stderr(),
        )?;
    }
    Ok(false)
}
```

`Err` short-circuits before any downstream `journal::clear_journal` call (all journal clears are on the success path -- see `recover.rs:962, 2022, 2251, 2343, 2422, 2434, 2475, 2676`), so the journal is preserved automatically. Update the docstring at `recover.rs:2684-2692` to reflect the new contract: "Best-effort except for `Suspended`, which returns an error so recover preserves the journal."

### 4. Rewire the progress poller

`cli/src/progress.rs:368-387`. Match arm becomes `NotStarted | Finished | Cancelled | Suspended { .. } => continue`. The foreground `btrfs replace start` thread carries the real outcome via `handle.join()`; the poller is display-only.

### 5. Tests

Every new test gets the AGENTS.md-required `// Intent / Why / Scenario` preamble.

#### Inline parser unit tests (`cli/src/parse/btrfs_replace_status.rs`)

- `canceled_zero_percent`: `"Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%, 0 write errs, 0 uncorr. read errs\n"` → `Cancelled`. Cite `replace.c:466-475` for output shape and `dev-replace.c:1051-1054` for the always-zero percent.
- `suspended_with_percentage`: `"Started on ..., suspended on ... at 12.5%, ..."` → `Suspended { pct: 12.5 }`. Cite `replace.c:476-485` and `dev-replace.c:1200-1251` (kernel trigger). Inline-only -- a VM-captured suspended fixture would require crash + missing-tgtdev-on-remount choreography that does not justify its maintenance cost; the upstream printf string is fully deterministic.
- `never_started`: `"Never started\n"` → `NotStarted`. Cite `replace.c:486-490` (note `skip_stats = 1`).
- `suspended_no_percent_token_falls_back_to_zero`: pin lenient parsing for the suspended prefix without a `%` token (defensive against future btrfs-progs format changes).
- Rename `not_started`, `no_operation_running`, `garbage_output_treated_as_none` to use `NotStarted` (no test additions, just variant rename).

#### Wait-loop display tests (`cli/src/recover.rs` test module, reusing `ReplaceStatusSequenceRunner` at `recover.rs:3349`)

- `wait_for_kernel_replace_emits_fail_on_canceled_returns_ok`: sequence `[Running 5%, Canceled]` → emits `[wait]`, `... 5.0%`, `[fail] ... canceled`. Returns `Ok(())`.
- `wait_for_kernel_replace_emits_fail_on_suspended_returns_err`: sequence `[Running 5%, Suspended 12.5%]` → emits `[wait]`, `... 5.0%`, `[fail] ... suspended at 12.5% ... cancel manually ...`. Returns `Err(RecoverError::Failed(_))` whose message matches the [fail] row body.
- `wait_for_kernel_replace_emits_fail_on_canceled_first_poll`: sequence `[Canceled]` → unconditional `[fail]` even though `[wait]` was never emitted. Returns `Ok(())`.

#### Recover-level abort test (`cli/src/recover.rs` test module, mirroring `recover_replays_resize_after_replace_via_mount_cycle` style at `recover.rs:10500+`)

- `recover_aborts_and_preserves_journal_on_suspended_replace`:
    - Setup: write a replace journal, mount the mock pool, mock `BtrfsReplaceStatus` to return `"Started on ..., suspended on ... at 50.0%, ..."`.
    - Drive `plan_recover` then `RecoverPlan::execute`.
    - Assert: returns `Err(RecoverError::Failed(msg))` where `msg.contains("suspended at 50.0%")` and `msg.contains("btrfs replace cancel")`.
    - Assert: `journal::load_journal` still returns the journal after the failure -- recovery did NOT clear it.
    - Assert: `relock_and_remount` mocks are NOT consumed -- the wait error short-circuits before the `RemountCycle` action runs.

(A symmetric "Cancelled diagnostic + continued recovery clears journal" test would duplicate `recover_replays_resize_after_replace_via_mount_cycle` -- skip in favor of the wait-loop display test plus the existing finished-path coverage.)

#### Golden fixture tests (`cli/tests/support/golden_common.rs`, included by `golden_nixos_25_11.rs` + `golden_nixos_unstable.rs`, using the `golden_test!` macro at `cli/tests/support/golden_common.rs:35`)

- `golden_btrfs_replace_status_never_started` -- fixture `btrfs-replace-status-never-started.txt`.
- `golden_btrfs_replace_status_finished` -- fixture `btrfs-replace-status-finished.txt`.
- `golden_btrfs_replace_status_canceled` -- fixture `btrfs-replace-status-canceled.txt`. Assert state is `Cancelled` (no pct field).

`Suspended` fixture intentionally omitted (see inline-only justification above).

#### Capture script changes (`tests/capture-tool-fixtures.py` and `.nix`)

A finished `btrfs replace` scratches the source device's superblock (`dev-replace.c:1012-1018`), which would break the existing script's later `mount /dev/mapper/braid-vdb` calls (`tests/capture-tool-fixtures.py:183, 194`). So replace captures must run AFTER all `braid-vdb`-dependent captures, but BEFORE the final `cryptsetup-status-inactive` close.

Required edits:

1. **`tests/capture-tool-fixtures.nix`**: add a third `emptyDiskImages` entry (`disk3`).
2. **`tests/capture-tool-fixtures.py`** -- new captures placed in this order:
    - **Early** (immediately after the initial `mount`, before any other btrfs operation, around line 30):
      ```python
      machine.succeed(
          f"btrfs replace status -1 {MOUNT}"
          f" > {FIXTURE_DIR}/btrfs-replace-status-never-started.txt"
      )
      ```
    - **Late** (after all balance / scrub / lsblk / device-stats captures and after `rm {MOUNT}/balancedata` at line 192, but before the cryptsetup-inactive close around line 196):
      ```python
      # Rebuild a clean filesystem for replace captures. The balance fixture
      # above intentionally leaves mixed data profiles behind, and that
      # topology can make dev_replace fail with ENOSPC before the canceled
      # fixture observes an in-flight state.
      machine.succeed(
          "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-vdb /dev/mapper/braid-vdc"
      )
      machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")

      # Format the third disk as LUKS, open as braid-vdd
      machine.succeed(f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode /dev/vdd -")
      machine.succeed(f"echo -n '{PASSPHRASE}' | cryptsetup open /dev/vdd braid-vdd -")

      # Write a large payload so `btrfs replace` runs long enough to observe
      # an in-flight window before cancel. The freshly rebuilt filesystem has
      # only metadata; a replace on that scale can finish faster than the
      # 0.05s polling cadence below, leading to a captured "finished" output
      # instead of "canceled".
      machine.succeed(f"dd if=/dev/urandom of={MOUNT}/replacedata bs=1M count=256")
      machine.succeed("sync")

      # Capture canceled: start replace vdb -> vdd in background, hard-assert
      # in-flight observation before cancel. Pattern mirrors
      # tests/module/ups-lb-during-replace.py:101-149 (PCT_RE +
      # saw_in_flight + saw_finished_too_early + bounded poll).
      PCT_RE = re.compile(r"(\d+(?:\.\d+)?)% done")
      def parse_replace_state(text):
          if "finished on" in text:
              return ("finished", 100.0)
          m = PCT_RE.search(text)
          if m:
              return ("running", float(m.group(1)))
          return ("idle", None)

      machine.execute(
          f"btrfs replace start -B 1 /dev/mapper/braid-vdd {MOUNT} "
          f"> /tmp/btrfs-replace-start.log 2>&1 &"
      )
      saw_in_flight = False
      saw_finished_too_early = False
      last_status = ""
      for _ in range(800):  # 40s budget
          ret = machine.execute(f"btrfs replace status -1 {MOUNT} 2>&1")
          last_status = ret[1]
          state, _ = parse_replace_state(last_status)
          if state == "running":
              saw_in_flight = True
              break
          if state == "finished":
              saw_finished_too_early = True
              break
          time.sleep(0.05)
      assert not saw_finished_too_early, (
          "btrfs replace finished before the canceled fixture could observe "
          "in-flight state. Payload too small or polling cadence too coarse. "
          "Last status:\n" + last_status
      )
      assert saw_in_flight, (
          "Never observed btrfs replace in-flight -- canceled fixture cannot "
          "be captured deterministically. Last status:\n" + last_status
      )
      # `btrfs replace cancel` returns once scrub cancel is requested, but
      # the kernel's CANCELED state transition runs in
      # `btrfs_dev_replace_finishing` (`dev-replace.c:937-939`). Status can
      # still report running for a tick before the flip. Poll until
      # "canceled on" appears; hard-fail on timeout or any unexpected state.
      machine.succeed(f"btrfs replace cancel {MOUNT}")
      saw_canceled = False
      saw_finished_too_early = False
      last_status = ""
      for _ in range(400):  # 20s budget
          ret = machine.execute(f"btrfs replace status -1 {MOUNT} 2>&1")
          last_status = ret[1]
          if "canceled on" in last_status:
              saw_canceled = True
              break
          if "finished on" in last_status:
              saw_finished_too_early = True
              break
          time.sleep(0.05)
      assert not saw_finished_too_early, (
          "btrfs replace transitioned to FINISHED after cancel -- the cancel "
          "raced kernel completion and the canceled fixture cannot be "
          "captured. Last status:\n" + last_status
      )
      assert saw_canceled, (
          "Kernel never transitioned to CANCELED within budget. Last "
          "status:\n" + last_status
      )
      machine.succeed(
          f"btrfs replace status -1 {MOUNT}"
          f" > {FIXTURE_DIR}/btrfs-replace-status-canceled.txt"
      )

      # Capture finished: rerun replace to completion. The previous tgtdev
      # allocation was destroyed on cancel; pass -f because braid-vdd may
      # carry residual fs signatures from the canceled run. -B blocks until
      # the kernel reports finished, so no in-flight observation needed here.
      machine.succeed(f"btrfs replace start -B -f 1 /dev/mapper/braid-vdd {MOUNT}")
      machine.succeed(
          f"btrfs replace status -1 {MOUNT}"
          f" > {FIXTURE_DIR}/btrfs-replace-status-finished.txt"
      )

      # Remove the payload before remaining teardown.
      machine.succeed(f"rm {MOUNT}/replacedata")
      ```

Order rationale: never-started runs first because it is sensitive to "no replace ever happened on this fs"; canceled and finished run last because the finished replace permanently swaps `vdb` out of the pool, after which the existing `mount /dev/mapper/braid-vdb` lines fail. The final `cryptsetup close braid-vdb` close at line 199 still works (LUKS mappers are independent of btrfs membership) -- the resulting cryptsetup-status-inactive fixture is unaffected. Closing `braid-vdd` for symmetry is left as a follow-up if the test diff surfaces it.

## Files modified

- `cli/src/parse/types.rs` -- enum split, drop wrapper
- `cli/src/parse/btrfs_replace_status.rs` -- parser arms + percent helper + 4 unit tests
- `cli/src/recover.rs` -- typed Result wait loop + propagating call site + docstring + 3 wait-loop display tests + 1 abort/journal-preservation test
- `cli/src/progress.rs` -- match arm widening
- `cli/tests/support/golden_common.rs` -- 3 shared golden test entries
- `tests/capture-tool-fixtures.py` -- 3 capture steps with reordering
- `tests/capture-tool-fixtures.nix` -- third `emptyDiskImages` entry

## Verification

```bash
just test-rust                       # unit + golden tests pass on existing fixtures
just capture-fixtures                # regenerate stable fixtures with the 3 new files
just test-rust                       # rerun against captured fixtures
just capture-all-fixtures-unstable   # regenerate unstable fixtures
just test-rust-unstable              # rerun against unstable
just test-vm                         # full VM suite -- catches any recovery-flow regressions
```

A passing `just test-rust` after fixture regeneration confirms parsing of real `nixos-25.11` btrfs-progs output for never-started, canceled, finished, and running states. The inline `suspended_with_percentage` test guards the upstream-deterministic format string for the one state that's impractical to capture. The new recover-level abort test pins the recover-blocking contract that motivated the typed Result.
