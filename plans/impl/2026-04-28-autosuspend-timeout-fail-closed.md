# plan: move `timeout -k` inside the `!` so autosuspend stays fail-closed under overrun

## Context

`modules/braid/auto-suspend.nix` configures the `BraidPool` external check that autosuspend runs every minute:

```nix
command = "${pkgs.coreutils}/bin/timeout 10 ${pkgs.bash}/bin/bash -c '! ${braidWrapped}/bin/braid idle'";
```

`timeout` is the outer process. If `braid idle` overruns past 10s, `timeout` kills bash and returns a non-zero timeout result itself. The `!` inversion never runs. autosuspend's `CommandActivity.check()` (`reference/autosuspend/src/autosuspend/checks/command.py:41-47`) returns `None` for any non-zero command result, and autosuspend treats that as "no activity", so the system is allowed to suspend.

This is the opposite of the fail-closed invariant documented at `docs/decisions/016-auto-suspend.md`:

> braid exit 2 (error) -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)

The known production overrun vector (`btrfs replace status` without `-1`) is already guarded by `replace-inhibits-suspend.py`, so this is a latent gap rather than a live bug. But the invariant should hold without depending on every future caller in `braid idle`'s probe set remembering to be timeout-safe.

### Coreutils behavior

GNU Coreutils' `timeout` starts the command, sends TERM at the main deadline, and returns non-zero for timeout outcomes (`reference/coreutils/src/timeout.c:18-30`, `reference/coreutils/src/timeout.c:581-629`). With `-k 2`, it also schedules SIGKILL two seconds after the initial timeout signal (`reference/coreutils/src/timeout.c:205-210`). That means the wrapper should not rely on exactly exit 124: TERM-only timeouts commonly return 124, while KILL escalation can produce a signal-style non-zero result.

### Residual behavior (out of scope for this fix)

`timeout(1)` cannot bound an uninterruptible kernel wait (process in `D` state on, e.g., a wedged btrfs ioctl): kill signals are queued until the syscall returns. Under that condition neither the buggy nor the fixed wrapper produces a timeout result on schedule -- the whole `subprocess.check_call` in autosuspend blocks until the kernel returns. That's a separate failure mode (autosuspend tick stalls; system stays awake by virtue of not deciding) and it is not what this plan claims to fix. The claim of this plan is narrower: signal-killable command overruns honor the fail-closed invariant.

## Change

**File:** `modules/braid/auto-suspend.nix`

Move `timeout` inside the bash `-c` expression, and use `-k 2` so TERM-resistant processes are escalated to KILL:

```nix
command = "${pkgs.bash}/bin/bash -c '! ${pkgs.coreutils}/bin/timeout -k 2 10 ${braidWrapped}/bin/braid idle'";
```

Behavior under signal-killable overrun now:

1. `braid idle` runs past 10s.
2. `timeout` sends TERM.
3. If still alive after two more seconds, `timeout` sends KILL.
4. `timeout` returns non-zero.
5. Bash applies `!`, exits 0.
6. autosuspend records activity and blocks suspend.

`coreutils/bin/timeout` keeps its fully-qualified path because PATH inside the autosuspend service is not guaranteed to include coreutils, and the braid wrapper does not export coreutils on PATH.

## Doc update

**File:** `docs/decisions/016-auto-suspend.md`

Extend the "Exit code inversion" section to record timeout placement and escalation as part of the invariant:

> - `braid idle` signal-killable overrun >10s -> `timeout -k 2 10` (inside bash) returns non-zero -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)

Add the rationale:

- `timeout` must be inside `bash -c` so its non-zero overrun result is inverted by `!`; an outer `timeout` would fail open.
- `-k 2` escalates TERM to KILL after two more seconds for TERM-resistant processes.
- Scope: this covers signal-killable overruns. Uninterruptible kernel waits (`D` state) are not bounded by `timeout(1)` and remain a separate failure mode; the autosuspend tick simply blocks, so the system stays awake by not deciding.

## Test

**File:** `tests/module/braid-auto-suspend.py`

Extend the existing module VM test with a behavioral subtest that pins the fail-closed-under-overrun invariant:

1. Extract the value after `command=` from the generated `[check.BraidPool]` section. The generated config contains a fully resolved `/nix/store/<hash>-braid-wrapped/bin/braid idle`, not the Nix expression `${braidWrapped}/bin/braid idle`.
2. Assert the generated command contains `bin/timeout -k 2 10`.
3. Write a TERM-resistant stub at `/tmp/braid-hang-stub`:
   ```sh
   #!/bin/sh
   trap "" TERM
   exec sleep 60
   ```
4. Substitute the resolved braid invocation with the stub via regex on the extracted command string. Use a regex like `/nix/store/[^ ]+/bin/braid idle` and assert that exactly one substitution occurred.
5. Run the modified command via `machine.execute`, wrapped in an outer `timeout -k 2 18` watchdog so the test cannot hang if the inner timeout is broken.
6. Assert exit code 0 (the `!` inverted the inner timeout's non-zero result) and elapsed wall time <15s (the inner `timeout -k 2 10` fired before the outer watchdog).

This tests the actual semantics of the configured command, not just its string structure. It fails if `timeout` is moved outside `bash -c`, if `-k 2` is removed, or if the command stops substituting the real `braid idle` invocation.

## Verification

- `just test-vm braid-auto-suspend` -- existing structural assertions still pass; new fail-closed-under-overrun subtest passes.
- `just test-vm` -- full VM suite is green (sanity).
- Manual sanity:
  ```sh
  bash -c '! timeout -k .2 .5 sh -c "trap \"\" TERM; exec sleep 5"'; echo $?
  timeout -k .2 .5 bash -c '! sh -c "trap \"\" TERM; exec sleep 5"'; echo $?
  ```
  The fixed pattern should return 0 quickly. The outer-timeout pattern should return non-zero.

## Out of scope

- No changes to `braid idle` itself or its callers. The fix is purely in the autosuspend wrapper.
- No retroactive audit of other shell pipelines in the module.
