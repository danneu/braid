# Fix `btrfs replace status` blocking + repro test in `replace-inhibits-suspend`

## Context

`cli/src/cmd.rs:579-582` builds `BtrfsReplaceStatus` as `btrfs replace status <mount>`, with no `-1` flag. Per `reference/btrfs-progs/cmds/replace.c:413-516` (`print_replace_status`), the `STARTED` arm of the switch does not set `prevent_loop`, so without `once = 1` (set by `-1`) the command **sleeps 1 s and re-reads the ioctl in a loop until the kernel reports `FINISHED`/`CANCELED`/`SUSPENDED`/`NEVER_STARTED`**. Until then it never returns.

Three callsites silently inherit that blocking behavior:

- `cli/src/idle.rs:83-89` — `cmd_idle` calls `BtrfsReplaceStatus` to ask "is a replace running?". When one is, the call blocks instead of immediately answering "yes". `braid idle` is the autosuspend integration point and must respond promptly. **This is the production-facing regression.**
- `cli/src/progress.rs:218-258` — `run_replace_with_progress` spawns the actual replace in a thread, then sleeps 1 s and calls `BtrfsReplaceStatus` to "poll" progress. The poll call blocks until the replace finishes; when it returns, output is `"Started on …, finished on …"`, the parser maps it to `ReplaceState::Finished`, the match arm hits `=> continue`, and **no progress line is ever rendered**. The polling loop is purely theatrical.
- `cli/src/recover.rs:435-461` — `wait_for_kernel_replace_to_finish`'s `last_pct` tracking and 200 ms sleep are dead code: each call only returns when the replace completes, so the body runs at most once before hitting `Finished | None` and returning.

The bug went unnoticed because `cli/src/parse/btrfs_replace_status.rs:78-89` test the `Running { pct }` branch with synthetic text (`"Started on 27.Feb 10:30:00, running, pid: 1234, 45.3% done, …"`) that real upstream output **never** produces. The actual `STARTED`-state output (replace.c:451-457, 497-501) is just `"45.3% done, 0 write errs, 0 uncorr. read errs"`.

`tests/cli/replace-inhibits-suspend.py:165-183` already drives a 3-disk pool through a real in-flight `btrfs replace start`, polls until the kernel reports a non-zero progress percentage, and is the natural extension point for an end-to-end repro: it has the inhibitor-held window between in-flight detection and replace completion in which we can call `braid idle` against a kernel-confirmed running replace.

Goal: make `BtrfsReplaceStatus` return immediately, and lock that behavior with a test that **fails when `-1` is dropped from `cmd.rs`** — i.e. asserts `braid idle` returns promptly and reports `replace running` while the replace is in flight, exercising the cmd helper + parser + idle path end-to-end.

## Recommended approach

### 1. Fix the cmd helper

`cli/src/cmd.rs:579-582` — add `"-1".into()` between `"status".into()` and `mount_point.0.clone()`:

```rust
CmdRequest::BtrfsReplaceStatus { mount_point } => CmdArgs {
    program: "btrfs",
    args: vec!["replace".into(), "status".into(), "-1".into(), mount_point.0.clone()],
},
```

This makes `cmd_replace_status` (replace.c:379-410) set `once = 1`, forcing one ioctl read + print and an immediate return. Every existing caller already assumes that semantic.

### 2. End-to-end repro test in `replace-inhibits-suspend.py`

Extend `tests/cli/replace-inhibits-suspend.py` with a new subtest in **Phase 4**, after the existing in-flight detection at line 183 and before the existing inhibitor assertion at line 187. The test already establishes a kernel-confirmed running replace at exactly that point; reuse that window.

The new subtest is the parser repro: it exercises the cmd helper (`BtrfsReplaceStatus` must include `-1` to return promptly), the parser (must extract `pct` from real upstream `STARTED`-state output), and the idle wiring (`cmd_idle` must report `BusyReason::ReplaceRunning`) — all in one assertion against live tool output.

```python
# --- Phase 4a: braid idle must return promptly + report replace running ---
#
# Intent: Pin that BtrfsReplaceStatus uses `-1`. Without it, btrfs replace
# status loops with sleep(1) on the STARTED state until the kernel reports
# FINISHED — see reference/btrfs-progs/cmds/replace.c:451-505. Every
# braid caller of BtrfsReplaceStatus (idle, progress, recover) inherits
# that blocking behavior; this assertion catches it via the autosuspend
# integration point, which is the production-facing regression.
#
# Why it exists: cli/src/cmd.rs:581 was missing the `-1` flag, so
# `braid idle` blocked indefinitely when a replace was in flight,
# preventing the autosuspend integration from making any decision at all.
# This test fails (timeout 124) without the cmd.rs fix.
#
# Scenario: replace is mid-flight (verified above). Operator's autosuspend
# daemon polls `braid idle`. The call must return within seconds and
# report `busy: replace running (X.Y%)`.
with subtest("braid idle returns promptly and reports replace running"):
    idle_exit, idle_out = machine.execute("timeout 5 braid idle 2>&1")
    print(f"=== braid idle during replace (exit {idle_exit}) ===")
    print(idle_out)
    assert idle_exit != 124, (
        "braid idle did not return within 5 s while a replace was in "
        "flight — BtrfsReplaceStatus is blocking. Check that "
        "cli/src/cmd.rs builds the command with the `-1` flag."
    )
    assert idle_exit == 1, (
        f"braid idle should report busy (exit 1) during a replace, "
        f"got exit {idle_exit}: {idle_out}"
    )
    assert "replace running" in idle_out.lower(), (
        f"braid idle did not report replace as the busy reason: {idle_out}"
    )
```

Why this is the parser repro test the user asked for, and why it is **strictly stronger than a golden fixture**:

- It runs `braid idle` end-to-end through the real `RealRunner`, which dispatches `CmdRequest::BtrfsReplaceStatus` via `to_argv()`. Drop `-1` from `cmd.rs:581` and the `timeout 5` wrapper kills the call → exit 124 → assertion fires.
- It exercises `parse_btrfs_replace_status` against bytes that **only `btrfs replace status -1` ever produces** — no synthetic fixture, no fictional `Started on …, running, pid: …` text. Drift in upstream output that breaks the parser surfaces here.
- It pins `cmd_idle`'s `BusyReason::ReplaceRunning` branch, which is the autosuspend integration the bug actually breaks in production.
- Zero new infrastructure: reuses the already-proven 3-disk pool + 400 MiB urandom payload + in-flight poll loop in `tests/cli/replace-inhibits-suspend.py`. No new disks, no `dm-delay` plumbing, no `progress-monitoring` restructuring.

### 3. Cheap unit-level guard on the cmd helper

`cli/src/cmd.rs` `mod tests` (around line 924) — add a tiny `to_argv()` assertion that locks `-1` into the `BtrfsReplaceStatus` request. This is a fast, deterministic complement to the VM test in step 2: if anyone drops `-1` from `cmd.rs:581` again, this test fails under `cargo test` in milliseconds, before CI even reaches the VM lane.

```rust
#[test]
// Intent: Lock the `-1` flag into BtrfsReplaceStatus's argv so the cmd
// helper always asks btrfs for a single status snapshot.
// Why: Without `-1`, btrfs replace status loops with sleep(1) on the
// STARTED state until the kernel reports FINISHED — see
// reference/btrfs-progs/cmds/replace.c:451-505. Every braid caller
// (idle, progress, recover) blocks for the entire duration of an
// in-flight replace, breaking the autosuspend integration in idle.rs.
// Scenario: a future refactor strips `-1` from cmd.rs:581 (e.g. while
// adding a continuous-poll variant). This test fails immediately.
fn btrfs_replace_status_includes_minus_one() {
    let req = CmdRequest::BtrfsReplaceStatus {
        mount_point: MountPoint("/mnt/storage".to_owned()),
    };
    let argv = req.to_argv();
    assert_eq!(argv.program, "btrfs");
    assert_eq!(
        argv.args,
        vec!["replace", "status", "-1", "/mnt/storage"],
        "BtrfsReplaceStatus must pass `-1` to avoid blocking until the \
         replace finishes — see reference/btrfs-progs/cmds/replace.c:451-505",
    );
}
```

### 4. Fix the misleading parser unit-test fixtures

`cli/src/parse/btrfs_replace_status.rs:8-11, 78-89, 113-124` — the doc comment claims output looks like `"Started on ...  45.3% done, ..."`, and the unit tests hand-roll the same fictional text. Replace both with the real `STARTED`-state format from `replace.c:451-501`:

- Doc comment (lines 8-11) — change the `Running` example to `"45.3% done, 0 write errs, 0 uncorr. read errs"`.
- `running_with_percentage` (lines 78-89) — replace fixture string to `"45.3% done, 0 write errs, 0 uncorr. read errs\n"`.
- `running_100_percent` (lines 113-124) — replace fixture string to `"100.0% done, 0 write errs, 0 uncorr. read errs\n"`.

Behavior is identical (the parser keys off `% done`), but the test file no longer documents output that production never produces. This is hygiene, not regression coverage; the regression coverage lives in steps 2 and 3.

### Out of scope (mention only)

- **Committed golden fixture for `BtrfsReplaceStatus`.** A `cli/tests/fixtures/nixos-25.11/btrfs-replace-status-running.txt` would let the always-on `just test-rust` lane track parser drift against real upstream output, but step 2 already gives end-to-end coverage that includes the parser, and adding fixture-export plumbing to `replace-inhibits-suspend.py` (write to `/tmp/fixtures`, `machine.copy_from_vm`, new `just` recipe) is a separable maintenance project. Not included unless the user asks for it.
- **Changes to `tests/progress-monitoring.py`.** Reusing `replace-inhibits-suspend.py` avoids the disk-count bump, `dm-delay` helper generalization, and subtest reordering that an earlier draft proposed for `progress-monitoring`.

## Files modified

- `cli/src/cmd.rs:579-582` — add `"-1".into()` to args.
- `cli/src/cmd.rs` `mod tests` (around line 924) — add `btrfs_replace_status_includes_minus_one` unit test asserting `to_argv()` contains `"-1"`.
- `cli/src/parse/btrfs_replace_status.rs:8-11, 78-89, 113-124` — replace synthetic fixture text + doc comment with real upstream `STARTED`-state format.
- `tests/cli/replace-inhibits-suspend.py` — insert a Phase 4a subtest between line 183 (end of in-flight detection) and line 187 (start of inhibitor assertion) that asserts `braid idle` returns within 5 s and reports `replace running`.

## Files reused, not modified

- `tests/cli/replace-inhibits-suspend.nix` — already provisions the 4-disk topology, braid binary, and `/etc/braid/config.json` with `mount_point = "/mnt/storage"` that `braid idle` reads. No nix-side changes needed.
- `cli/src/parse/btrfs_replace_status.rs` `parse_btrfs_replace_status` — already correct; only its doc + tests are misleading.
- `cli/src/idle.rs:83-89` `cmd_idle` — already wired to surface `BusyReason::ReplaceRunning { pct }`; the bug was upstream of it in the cmd helper.

## Verification

The two new tests catch the bug at different layers; both should be added together so neither is the only line of defense.

1. **Reproduce the bug at both layers first.** With `cli/src/cmd.rs` unchanged:
   - Apply only the unit test from step 3 and run `cargo test --manifest-path cli/Cargo.toml -p braid-cli btrfs_replace_status_includes_minus_one`. It must fail with the assertion message.
   - Apply only the VM subtest from step 2 and run `just test-vm replace-inhibits-suspend`. It must fail with `idle_exit == 124`, proving the test catches the actual blocking behavior end-to-end.
   - If either test passes without the cmd fix, the test is wrong and the plan is wrong — stop and revisit.
2. Apply the `cmd.rs` fix from step 1.
3. `cargo test --manifest-path cli/Cargo.toml -p braid-cli btrfs_replace_status_includes_minus_one` — must now pass.
4. `just test-vm replace-inhibits-suspend` — must pass; new subtest reports `braid idle` exit 1 + "replace running (X.Y%)".
5. Apply the parser unit-test fixture cleanup from step 4.
6. `cargo test --manifest-path cli/Cargo.toml -p braid-cli btrfs_replace_status` — exercises the updated parser unit tests with real upstream text.
7. `just test-vm braid-idle replace-2disk-pool replace-sequential` — sanity-check the existing replace + idle integration tests under the new cmd args.
8. `just test-rust` — full Rust unit + golden suite. This catches **parser-level** regressions only (the existing golden fixtures exercise other parsers, and the new step-3 unit test guards the cmd helper). It does **not** exercise the live `braid idle` path; the actual end-to-end protection against this bug lives in the VM subtest from step 2 plus the cmd-helper unit test from step 3.

## Notes / risks

- **No backwards compatibility concerns**: per `AGENTS.md`, braid is unreleased.
- **The 5 s `timeout` budget** is generous: in a healthy VM `braid idle` returns in <1 s. The 5 s window absorbs LUKS/findmnt/scrub/balance check overhead but is still tight enough to fail loudly when `BtrfsReplaceStatus` blocks for the duration of a 400 MiB replace (~tens of seconds on VM disks).
- **Subtest ordering inside replace-inhibits-suspend.py is load-bearing**: the new assertion must run *after* the kernel reports in-flight progress (line 183) so that the bug's blocking branch is actually exercised, and *before* the existing `wait_until_succeeds("…finished on…")` (line 209) so the replace is still in flight when `braid idle` runs. Place it as Phase 4a, immediately after Phase 3.
- **The fix unblocks autosuspend in production today.** After this fix, `braid idle` correctly reports `replace running (X.Y%)` while a replace is in flight, instead of blocking the autosuspend check.
