# Plan: collapse `braid idle` probe-failure exit code from 2 to 1

## Context

`braid idle` currently has three exit codes documented at `cli/src/main.rs:44`:
`0` = idle/offline, `1` = busy, `2` = error. autosuspend can only see *zero*
vs *non-zero* through the bash `!` operator at
`modules/braid/auto-suspend.nix:86`:

```
bash -c '! timeout -k 2 10 braid idle'
```

`!` inverts any non-zero to zero, so exit 1 (busy) and exit 2 (probe error)
are indistinguishable to autosuspend. The "Rust exit 2 -> bash `!` -> exit 0
= activity" chain documented at `docs/decisions/016-auto-suspend.md:36` is
fail-closed only by accident of bash semantics, not by anything the Rust
type system enforces.

**Scoped change:** fold *pool-state probe failures* (parse errors, scrub
subprocess errors, fsid probe errors, sysfs read errors, mountinfo errors)
into `IdleResult::Busy(BusyReason::Unknown(...))` so they exit 1 instead of
2. **Setup/config errors at `cli/src/main.rs:540-545` still exit 2** -- those
happen before `cmd_idle` runs, are not pool-state probe failures, and have
distinct meaning to a direct human user (your config is busted vs your pool
state is unknowable). They keep behaving the same and are still fail-closed
under autosuspend via the same `!` inversion.

**New documented contract:**

| Exit | Meaning |
|------|---------|
| 0 | Pool is idle, OR pool is offline (not mounted) |
| 1 | Pool is busy (running op) OR pool state could not be determined (probe failure) |
| 2 | Setup error -- config could not be read |

Rationale:

- Pool-state probe failure and "pool is busy doing something" are
  indistinguishable from autosuspend's perspective and from the user's
  perspective ("don't suspend, something is up"). Collapsing them removes
  the three-hop-inversion fragility for the common case.
- Config errors are a different class of failure (operator misconfigured
  braid, not the pool), they predate `cmd_idle`, and a direct human user
  benefits from the distinct exit code + stderr `error:` prefix.
- VM tests already encode this: `tests/cli/braid-idle.py:54-56` asserts
  `exit_code in [0, 1]` and explicitly rejects exit 2 during a probe race.

The `bash -c '! timeout -k 2 10 braid idle'` wrapper keeps working unchanged
for both classes of failure -- both `1` and `2` invert to `0`, blocking
suspend. The signal-killable timeout fail-closed behavior at
`016-auto-suspend.md:37` is unaffected.

## Changes

### 1. `cli/src/idle.rs`

- Add `BusyReason::Unknown(String)` (lines 18-34). Carries the underlying
  error message verbatim so direct users keep the diagnostic.
- Add `BusyReason::Unknown(msg) => write!(f, "unknown ({msg})")` to the
  `Display` impl (lines 36-50).
- Delete the `IdleError` enum (lines 52-67) entirely.
- Change `cmd_idle` (lines 69-110) to return `IdleResult` (no `Result`
  wrapper). Each `?` site that previously propagated to `IdleError` now
  short-circuits to `IdleResult::Busy(BusyReason::Unknown(e.to_string()))`:
  - `is_btrfs_mounted(...)?` (line 75) -- `MountInfoError`
  - `runner.run(BtrfsScrubStatus)?` (line 84) -- `CmdError`
  - `parse_btrfs_scrub_status(...)?` (line 85) -- `ParseError`
  - `probe_fsid(...)?` (line 102) -- `ProbeError`
  - `Err(ExclusiveOpError::Read | Unrecognized)` arm (lines 106-108)
- `is_btrfs_mounted` helper (lines 137-145) returns
  `Result<bool, MountInfoError>` directly so the caller can match and
  convert. The fail-closed comment block needs the "exit 2" reference
  rewritten to "exit 1 (Busy::Unknown)".

### 2. `cli/src/main.rs`

- **Line 44** (clap help): change to
  `Check if pool is idle (no scrub or btrfs exclusive operation): exit 0 = idle, exit 1 = busy or probe failure, exit 2 = setup error`.
- **Lines 540-545**: unchanged -- config errors still exit 2.
- **Lines 549-566**: collapse `cmd_idle`'s `Result` arms to three `IdleResult`
  arms:

  ```rust
  match braid_cli::idle::cmd_idle(&runner, &fs, config.mount_point()) {
      IdleResult::PoolOffline   => { println!("idle: pool is offline"); std::process::exit(0); }
      IdleResult::Idle          => { println!("idle: pool is idle");    std::process::exit(0); }
      IdleResult::Busy(reason)  => { println!("busy: {reason}");        std::process::exit(1); }
  }
  ```

`print_cli_error` (lines 758-764) is no longer reached from this command.

### 3. Tests in `cli/src/idle.rs`

Five tests change from `Err(IdleError::*)` assertions to
`IdleResult::Busy(BusyReason::Unknown(_))`:

| Test | Line | Old | New |
|---|---|---|---|
| `error_on_unrecognized_exclop` | 484 | `IdleError::Exclop(_)` | `IdleResult::Busy(BusyReason::Unknown(_))` |
| `error_on_sysfs_read_failure` | 495 | `IdleError::Exclop(_)` | `IdleResult::Busy(BusyReason::Unknown(_))` |
| `error_on_scrub_probe_failure` | 532 | `result.is_err()` | `matches!(result, IdleResult::Busy(BusyReason::Unknown(_)))` |
| `mountinfo_read_failure_is_not_pool_offline` | 554 | `IdleError::MountInfo(_)` | `IdleResult::Busy(BusyReason::Unknown(_))` |
| `mountinfo_malformed_target_line_is_not_pool_offline` | 574 | `IdleError::MountInfo(_)` | `IdleResult::Busy(BusyReason::Unknown(_))` |

Rename the last two from `_is_not_pool_offline` to `_is_busy_unknown` -- the
invariant is now stronger ("must be Busy::Unknown", not just "must not be
PoolOffline").

The `Why`/`Scenario` comment blocks stay; they still document fail-closed
behavior, only the result-shape changes.

### 4. New VM subtest in `tests/cli/braid-idle.py`

After the pool is mounted in the existing test, add a subtest that forces a
probe failure and asserts the new contract behaviorally (not just at the
type level).

**PATH-shim caveat:** `braid` is wrapped at `flake.nix:48` with
`makeWrapper --prefix PATH : ${toolPath}`, which forcibly prepends
btrfs-progs to PATH. A plain `PATH=/tmp/btrfs-stub:$PATH braid idle`
invocation never reaches the shim -- toolPath's `btrfs` wins. The same
issue is documented and worked around in `tests/cli/braid-remove-softwarn.py:56-72`.

The new subtest follows the same pattern:

1. Resolve the unwrapped binary from the wrapper script:
   ```python
   braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
   wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
   m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
   assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
   unwrapped_braid = m.group(1)
   ```
2. Write `/tmp/btrfs-stub/btrfs` that delegates to the real `btrfs` for
   everything except `scrub status`, which `exit 1`s. Capture the real
   path with `real_btrfs = machine.succeed("command -v btrfs").strip()` so
   the stub can pass through other invocations cleanly.
3. Run `PATH=/tmp/btrfs-stub:$PATH {unwrapped_braid} idle` and capture
   exit code + stdout.
4. Assert exit code == 1 (not 0, not 2).
5. Assert stdout starts with `busy: unknown (` -- preserves the diagnostic.
6. Remove the stub and verify a subsequent (wrapped) `braid idle` returns
   to its prior behavior (idempotency check).

This test pins the *behavioral* claim of the refactor (probe failure ->
exit 1 with diagnostic) -- the unit-test changes in `idle.rs` only pin the
return type. Without the unwrapped-binary indirection, the test would
silently pass while never exercising the shim.

### 5. `manual/commands/idle.md`

- **Lines 30-36** (exit-codes table): rewrite to match the new contract:

  | Exit | Meaning |
  |---|---|
  | 0 | Pool is idle, or pool is offline |
  | 1 | Pool is busy (running op) or pool state could not be determined |
  | 2 | Setup error -- config could not be read |

- **Lines 38-49** (busy-reason list): add `busy: unknown (<error>)` to the
  list, with one sentence: "Printed when a probe failed (parser regression,
  sysfs read error, etc). The parenthesized message is the underlying
  error."
- **Lines 73-75** (autosuspend exit-code walkthrough): rewrite to
  `0 (idle) -> ! -> 1 -> allow`, `1 (busy or probe failure) -> ! -> 0 -> block`,
  `2 (setup error) -> ! -> 0 -> block (fail-closed)`.

### 6. `docs/decisions/016-auto-suspend.md`

- **Line 27**: "Fail-closed behavior (exit 2 on any probe error -> block
  suspend)..." -> "Fail-closed behavior (probe failures map to
  `Busy(Unknown)` -> exit 1 -> block suspend; setup/config errors stay at
  exit 2 and also block via `!`)..."
- **Line 36**: change the bullet to "braid exit 1 (busy or probe failure)
  -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)" and add a
  sibling bullet "braid exit 2 (setup error) -> `!` -> exit 0 -> autosuspend:
  block suspend (fail-closed)".
- **Line 49**: "...propagate as `IdleError::MountInfo`, surface as exit
  2..." -> "...surface as `Busy(BusyReason::Unknown)`, exit 1, block
  suspend."
- **Line 51**: "...becomes `IdleError::Probe` and exit 2" -> "...becomes
  `Busy(BusyReason::Unknown)` and exit 1."

## Files modified

- `cli/src/idle.rs`
- `cli/src/main.rs` (lines 44 and 549-566)
- `cli/src/idle.rs` tests (5 tests + 2 renames)
- `tests/cli/braid-idle.py` (new probe-failure subtest)
- `manual/commands/idle.md` (exit table + busy-reason list + autosuspend walkthrough)
- `docs/decisions/016-auto-suspend.md` (lines 27, 36, 49, 51)

## What does not change

- `modules/braid/auto-suspend.nix:86` (`bash -c '! timeout ...'` wrapper).
- `cli/src/main.rs:540-545` (config-read exit 2).
- `cli/src/main.rs:758-764` (`print_cli_error`).
- `tests/cli/replace-inhibits-suspend.py:142-152` -- already asserts
  `idle_exit == 1` for busy.
- `tests/module/braid-auto-suspend.py` -- tests the bash wrapper, exit-code
  inversion is unchanged.
- `cli/src/probe.rs`, `cli/src/parse.rs`, `cli/src/cmd.rs`,
  `cli/src/preflight.rs`, `cli/src/mount_check.rs` -- their error types are
  unchanged; only `idle.rs` stops wrapping them.

## Verification

1. `just test-rust` -- the five updated unit tests pass; no other Rust test
   references `IdleError`.
2. `just test-vm braid-idle` -- exercises the existing assertions plus the
   new probe-failure subtest (exit 1 + `busy: unknown (...)` on stdout).
3. `just test-vm replace-inhibits-suspend` -- exercises `idle_exit == 1`
   during device replace (unchanged).
4. `just test-vm braid-auto-suspend` -- exercises the bash wrapper
   end-to-end; verifies fail-closed behavior under the new exit codes.
5. `cargo build -p braid-cli` to confirm no stale `IdleError` references
   remain.
6. `braid idle --help` manually inspected to confirm the new help string at
   `cli/src/main.rs:44` is current.

## Out of scope

- "Finding #3 fail-open on timeout" referenced in the original issue -- the
  existing `timeout -k 2 10` placement *inside* `bash -c` already inverts via
  `!`, so there's no fail-open seam here today. If a separate finding alleges
  otherwise, address it in its own plan.
- Splitting `BusyReason::Unknown` into typed sub-variants (e.g.
  `Unknown::Parse`, `Unknown::Mount`). The `String` payload is sufficient --
  callers don't branch on the underlying cause; only humans read it.
- Setup/config error handling. `cli/src/main.rs:540-545` keeps its current
  behavior (eprintln + exit 2). Changing config-error UX is a separate
  question.
