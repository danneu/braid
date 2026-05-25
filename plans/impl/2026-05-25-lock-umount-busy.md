# Lock umount retry-on-EBUSY

## Context

`braid lock` currently runs a single `umount` attempt at `cli/src/lock.rs:542-566`. If it returns EBUSY, the lock fails immediately and the user must rerun the command.

On the caja NAS today we observed this race: braid stopped `samba-smbd.service` via `systemctl stop` (succeeded, cgroup empty, "Deactivated successfully" logged), fired umount in the same second, and got `EBUSY: target is busy`. 50 seconds later a second `braid lock` succeeded cleanly. The kernel-side fd/inode release after smbd exit is racy with an immediately-following umount, especially under active SMB read traffic.

The fix: retry umount up to 3 times with 500ms backoff, mirroring the existing `close_mapper_with_retry` pattern at `cli/src/mapper_close.rs:22-66`. The success path pays zero latency; the all-fail path adds at most ~1 second before reporting the existing error. This will not save a real busy condition (a shell with cwd in the pool, an active rsync) -- those will still fail, just 1 second later.

## Design

Add a private helper `umount_with_retry` to `cli/src/lock.rs` and use it from `LockPlan::execute`. The helper mirrors `close_mapper_with_retry` in shape (1..=N loop, sleep-on-retry, warn-log per retry, return Err on exhaustion) but classifies EBUSY via the existing `umount_stderr_is_busy(stderr)` at `cli/src/lock.rs:330-336` because umount exits 32 generically -- the diagnostic stderr segment is load-bearing, not the exit code.

The helper returns `Ok(())` on success and `LockError` on failure. The caller partitions failure modes: a `LockError::Cmd(_)` (command-execution failure from `runner.run`) propagates immediately as today; anything else is the existing warn-and-continue path (set `umount_error`, attempt mapper close).

**Decision 022 compliance** (typed-work-plan / preview model -- AGENTS.md mandates reading before modifying mutating-command execution): the retry happens at execution time only. The dry-run preview still emits a single unmount `Step` (no change to `compile_lock_steps`), and the retry path re-runs the same `CmdRequest::Umount` without adding new step variants. This mirrors the `close_mapper_with_retry` precedent that Decision 022 explicitly blesses for `lock` (Scope, lines 83-89).

**Scope: this change is limited to `LockPlan::execute`.** `recover.rs:3401` issues its own `CmdRequest::Umount` inside `relock_and_remount` and treats EBUSY as a hard `RecoverError::Failed`. That path is intentionally left un-retried -- it drains internal `dev_replace` work, not external SMB/NFS consumers, so the post-`systemctl stop` kernel-fd-release race that motivates this change does not apply. `umount_with_retry` stays private to `lock.rs`; sharing it now would be speculative.

### Critical file

**`cli/src/lock.rs`** -- all code changes are in this file.

1. **Add retry constants** near the top of the file (mirror `mapper_close.rs:7-8`):

   ```rust
   const UMOUNT_RETRY_ATTEMPTS: u32 = 3;
   const UMOUNT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
   ```

2. **Add `build_umount_error`** -- extract the existing error+hint construction from `execute` into a helper so the retry loop and any future caller produce byte-identical messages. This is the format already at `lock.rs:546-557`. Per the repo rule on doc comments for new top-level Rust items (AGENTS.md), prefix it with a short `///` explaining the boundary:

   ```rust
   /// Centralize the umount failure message + lsof/fuser hint wording so
   /// retry exhaustion and non-busy-fail paths preserve a single
   /// operator-facing contract.
   fn build_umount_error(mount_point: &MountPoint, exit_status: i32, stderr: &str) -> LockError {
       let mut msg = format!("umount {mount_point} failed (exit {exit_status}): {stderr}");
       if umount_stderr_is_busy(stderr) {
           msg.push_str(&format!(
               "\nhint: a process may be using files on the mount. \
                Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it."
           ));
       }
       LockError::Failed(msg)
   }
   ```

3. **Add `umount_with_retry`** -- the loop. Doc comment explains why it exists (kernel-side fd release race after `systemctl stop` of BoundBy SMB/NFS consumers). Uses `emit_status(&status_line(...))` for the retry warn line, matching `close_mapper_with_retry` at `mapper_close.rs:56-62`, so the line is reachable via `status_tag::testing::capture_with_color` in unit tests.

   ```rust
   fn umount_with_retry<R, S>(
       runner: &R,
       sleeper: &S,
       mount_point: &MountPoint,
       color_enabled: bool,
   ) -> Result<(), LockError>
   where
       R: CommandRunner,
       S: Sleeper + ?Sized,
   {
       for attempt in 1..=UMOUNT_RETRY_ATTEMPTS {
           let result = runner.run(&CmdRequest::Umount {
               mount_point: mount_point.clone(),
           })?;
           if result.exit_status == 0 {
               return Ok(());
           }
           let stderr = result.stderr.trim();
           if !umount_stderr_is_busy(stderr) {
               return Err(build_umount_error(mount_point, result.exit_status, stderr));
           }
           if attempt == UMOUNT_RETRY_ATTEMPTS {
               return Err(build_umount_error(mount_point, result.exit_status, stderr));
           }
           emit_status(&status_line(
               StatusTag::Warn,
               color_enabled,
               &format!(
                   "umount {mount_point} busy, retrying ({attempt}/{UMOUNT_RETRY_ATTEMPTS})..."
               ),
           ));
           sleeper.sleep(UMOUNT_RETRY_DELAY);
       }
       unreachable!()
   }
   ```

   Note: the inline `eprint!` calls in `LockPlan::execute` (the `[wait]`, `[ok]`, `[fail]` lines around the umount) stay on `eprint!` -- the helper-vs-inline split mirrors today's mapper-close pattern (mapper_close.rs uses `emit_status` because it has its own unit tests; lock.rs's inline lines stay on `eprint!` because they are exercised by integration tests).

4. **Replace the entire `if self.pool_was_mounted { ... }` umount block** in `LockPlan::execute` (currently `lock.rs:534-611`: the `if` opener through its closing brace). The `[wait]` line above the existing runner.run call, the `if umount_result.exit_status != 0` failure arm, and the `else` arm holding the `[ok]` line and the btrfs forget block all collapse into the match below. Do not leave the old `else { ... }` in place -- the new `Ok(())` arm absorbs it.

   ```rust
   if self.pool_was_mounted {
       eprint!("{}", line(StatusTag::Wait, &format!("pool: unmounting {mount_point}...")));
       match umount_with_retry(runner, sleeper, mount_point, color_enabled) {
           Ok(()) => {
               eprint!("{}", line(StatusTag::Ok, &format!("pool: unmounted {mount_point}")));
               // existing btrfs device scan --forget block (today at lock.rs:573-609)
               // moves here verbatim -- same forget_paths/retain/match shape.
           }
           Err(err @ LockError::Cmd(_)) => return Err(err),
           Err(err) => {
               eprint!("{}", line(StatusTag::Fail, &format!("{err}")));
               eprint!("{}", line(StatusTag::Warn, "attempting to close LUKS mappers despite umount failure..."));
               umount_error = Some(err);
           }
       }
   }
   ```

   The `Err(err @ LockError::Cmd(_)) => return Err(err)` arm preserves today's behavior at `lock.rs:542`: a command-execution failure from `runner.run(...)` (the existing `?` path) returns immediately without attempting mapper close.

### Reused existing utilities

- `Sleeper` trait at `cli/src/progress.rs:11-29` (already injected into `LockPlan::execute` via the `S: Sleeper` generic).
- `umount_stderr_is_busy` at `cli/src/lock.rs:330-336` (existing classifier, unchanged).
- `emit_status` / `status_line` / `StatusTag::Warn` from `cli/src/status_tag.rs` (the `emit_status` indirection routes the line through the thread-local test capture).
- `MockRunner::with_output_sequence` for queuing multiple successive responses to the same `CmdRequest::Umount` in tests.
- `status_tag::testing::capture_with_color` for asserting the retry warn line content.

### Test changes -- `cli/src/lock.rs` test module

Two existing tests updated; four new tests added. Use the test preamble format at `docs/dev/testing.md`.

**Update existing** (each currently stages a single umount failure via `lock_umount_failed_runner` / single `with_output`):

- `lock_umount_busy_fails` (around `lock.rs:1784`) -- switch from a single umount output to `with_output_sequence` of **3** busy outputs; assert the error still surfaces and (new) assert umount was called exactly 3 times.
- `lock_umount_busy_includes_hint` (around `lock.rs:1831`) -- same: 3 busy outputs, assert hint text in error.

**Add new:**

- `lock_umount_busy_retry_succeeds_on_second_attempt` -- stage `[busy, ok]` via `with_output_sequence`. Wrap the call in `status_tag::testing::capture_with_color(false, || ...)` and assert (a) lock succeeds end-to-end, (b) mappers close normally, (c) the captured output contains the `[warn] umount /mnt/storage busy, retrying (1/3)...` line.
- `lock_umount_non_busy_failure_does_not_retry` -- stage a single umount with non-busy stderr (e.g. an invented "device not configured" stderr that fails `umount_stderr_is_busy`). Assert exactly 1 umount call and that lock fails immediately with no retry warn in captured output.
- `umount_with_retry_sleeps_prod_delay_between_busy_attempts` -- mirror the existing `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts` at `lock.rs:3553`. Use an inline `RecordingSleeper` (Mutex<Vec<Duration>>); stage 3 busy umount outputs; assert (a) returns `Err(LockError::Failed(_))`, (b) recorded sleeps == `UMOUNT_RETRY_ATTEMPTS - 1`, (c) each sleep == `UMOUNT_RETRY_DELAY`, (d) `UMOUNT_RETRY_DELAY == Duration::from_millis(500)`. This locks the production delay value so a regression that zeroes or removes it fails CI even though all other tests use `LockNoopSleeper`.
- `lock_umount_cmd_error_bubbles_immediately_without_mapper_close` -- use a runner that returns `Err(CmdError::MissingMock)` for `CmdRequest::Umount` (no `with_output` for umount). Assert (a) `cmd_lock_impl` returns `Err(LockError::Cmd(_))`, (b) no `CmdRequest::CryptsetupClose` calls were recorded. This locks the `Err(err @ LockError::Cmd(_)) => return Err(err)` arm against silent regression.

Use `LockNoopSleeper` (already exported from `test_fixtures`) everywhere except the delay-timing test.

### Live-tool coverage

The classifier `umount_stderr_is_busy` is unchanged, and a live-tool test already exists at `tests/cli/braid-lock-umount-busy.py` (registered in `flake.nix:507` as `braid-lock-umount-busy`). It uses `tail -f` to hold the mount busy, runs `braid lock`, and asserts failure with the lsof/fuser hint; then kills the blocker and asserts a follow-up `braid lock` succeeds and closes all mappers.

This test remains valid under the retry change: with a persistent blocker, the retry loop will run all 3 attempts (~1s total) and then surface the same failure with the same hint. The test does not assert call counts or timing, only failure-then-hint, so it passes as-is.

### Docs update

**`docs/commands/lock.md`** -- two small edits:

- Step 3 in "What happens under the hood" (`lock.md:37`): change "Unmounts the btrfs filesystem" to "Unmounts the btrfs filesystem, retrying up to 3 times if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors)". Leave step 5 (`lock.md:39`) untouched -- it documents the distinct mapper-close retry and already mentions "retrying up to 3 times".
- "Error handling" section (`lock.md:58`): change "If unmount fails (e.g. a process has files open on the mount)" to "If unmount fails after 3 retry attempts (e.g. a process has files open on the mount)" so the documented behavior matches.

## Verification

1. **Unit tests**: `just test-rust` -- covers the four new tests plus the two updated busy tests.
2. **Live-tool VM test**: `just test-vm braid-lock-umount-busy` -- exercises the real umount EBUSY classifier and the lsof/fuser hint emission against real `umount(8)` output.
3. **Manual reproduction of the original failure**: on caja (the NAS), mount `/mnt/storage/creepy` from a Mac via SMB, start an active video read stream, then `sudo braid lock`. Expected: either a clean lock or one or two `[warn] umount /mnt/storage busy, retrying (N/3)...` lines followed by `[ok] pool: unmounted /mnt/storage`.

(The non-busy umount-failure branch is covered exclusively by the `lock_umount_non_busy_failure_does_not_retry` unit test; it cannot be reproduced manually because `plan_lock` gates `pool_was_mounted` on `mountpoint -q` and skips the umount call entirely when the pool is already unmounted.)
