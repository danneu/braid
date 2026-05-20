# Plan: Pin read-only diagnostics (`status`, `doctor`) to non-acquiring under held pool lock

## Context

`docs/principles.md:65-67` (Principle 12) and `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:581-583` carve `status`, `doctor`, `idle`, `tui`, `ups`, `help`, bare `discover`, and the internal `scrub-*` commands out of the locked-command list. Rust dispatch is the serialization boundary: every mutating arm in `cli/src/main.rs` calls `acquire_pool_or_exit(&pool_lock)` (e.g. `Add` at line 405, `Remove` at 452, `Unlock` at 576, `Recover` at 864) but `Status` (`cli/src/main.rs:539-561`) and `Doctor` (`cli/src/main.rs:562-574`) intentionally do not.

The existing test `tests/module/pool-lock-precedes-state-read.py` covers one half of Principle 12 (locked arms acquire BEFORE reading config/membership/journal/probe/passphrase). No test covers the other half (exempt arms NEVER acquire). The closest related coverage is `tests/module/pool-lock-discover-contention.py:54-58`, which only asserts that bare `discover` stays unlocked -- nothing pins `status` or `doctor`.

The risk: a future refactor that copy-pastes `let _pool_guard = (!args.dry_run).then(|| acquire_pool_or_exit(&pool_lock));` into the `Doctor` or `Status` arm -- the same shape every mutating arm uses -- would pass CI but silently break the "doctor is always available during an incident" invariant. `status` and `doctor` are the two operator-facing diagnostics; protecting their availability is the architectural point of the exempt list.

This change adds the missing regression test.

## Plan

Add a new NixOS VM test that holds `/run/braid-pool.lock` externally with a long-lived holder, runs `braid status`, `braid status --json`, `braid doctor`, `braid doctor --json`, and asserts (a) none of them produces the contention message, (b) none of them hangs to timeout, and (c) each reaches its diagnostic path (positive output sentinel). Also update Principle 12 in `docs/principles.md` to explicitly name `status` and `doctor` as lock-free diagnostics, so the architectural invariant the test pins is documented in the active architecture doc, not just an implementation plan.

### Files to create

- **`tests/module/pool-lock-readonly-bypass.py`** -- test script (~50 lines).
  - Preamble follows the convention in `AGENTS.md` ("Test Conventions" section): Intent / Why it exists / Scenario.
  - **Holder pattern: long-lived shared holder**, modeled on `tests/module/alert-state-lock.py:114-132` (`start_lock_holder` / `stop_lock_holder`). Holder sleeps 60s under `nohup flock -x 9`, is killed explicitly after all four commands complete. This is critical for correctness: the holder must outlive every command's timeout window, otherwise a future regression that introduces a bounded-wait acquisition (e.g. the `ack`-style 10s wait shape from `cli/src/main.rs:774`) could simply wait out the holder and produce normal output, passing the test while violating the invariant. The `with_holder()` helper from `pool-lock-precedes-state-read.py:23-36` does the opposite (holder dies after `hold_secs`), so it is the wrong shape here.
  - **Invocation helper: capture stdout and stderr separately**, modeled on `tests/cli/braid-doctor-foreign-luks-uuid.py:63` (which uses `2>/tmp/braid-doctor.err`). The helper redirects stdout and stderr to two distinct temp files, returns `(rc, stdout, stderr)` strings, and runs under `timeout 5`. Do NOT merge streams with `2>&1`: the JSON-mode commands print machine-readable JSON to stdout (`cli/src/doctor.rs:1289` `println!`, `cli/src/status.rs` `--json` serialization) while error/contention text goes to stderr (`print_cli_error` at `cli/src/main.rs:1070-1076` uses `eprintln!`; the contention message specifically at `cli/src/main.rs:990` is `eprintln!("{}", PoolLockError::AlreadyHeld)`). If a future regression on doctor causes `cmd_doctor` to also print an "error: ..." line to stderr while still emitting valid JSON to stdout (the Fail path), a merged-stream helper corrupts the JSON and the test fails for the wrong reason; a stdout-only helper misses the stderr-bound contention message entirely. Separate-stream capture is the only shape that supports both assertions cleanly.
  - For each of `braid status`, `braid status --json`, `braid doctor`, `braid doctor --json`:
    - Invoke under the held lock with the separate-stream helper. The command timeout (5s) is well under the 60s holder window, so any acquisition path -- non-blocking fail-fast, bounded-wait, or blocking-forever -- is caught.
    - Assert `rc != 124` (didn't hang waiting for the lock).
    - Assert `"another braid operation is already in progress"` is NOT in `stdout + stderr` (concatenated). Checking the union of both streams is robust: the message goes to stderr today, but a future change in error-printing helper could move it. (catches non-blocking fail-fast and bounded-wait-then-timeout regressions).
    - **Assert a positive output sentinel proving the diagnostic path was reached** (catches the case where a future error wording change makes the negative assertion pass vacuously). All positive sentinels live on stdout (both human renderers use `print!` / `println!`; JSON mode uses `println!`):
      - `braid status` (human) -- `stdout` contains `not mounted` (sourced from `cli/src/status.rs:41` `StatusCode::NotMounted => "not mounted"`).
      - `braid status --json` -- `json.loads(stdout)` parses and `obj["status"] == "not_mounted"` (matches `cli/src/status.rs:1271`).
      - `braid doctor` (human) -- `stdout` contains `config file` (the check label rendered by `cli/src/doctor.rs:1236` for the always-present `check_config_file`).
      - `braid doctor --json` -- `json.loads(stdout)` parses and has a check with `name == "config_file"` (matches `cli/src/doctor.rs:190-235,1552`).
  - Do NOT assert a specific success exit code. The pin is structural -- "didn't block on contention, reached the diagnostic path" -- not "doctor's environmental checks all passed." Positive sentinels prove the path reached; pinning `rc == 0` would create flakiness if doctor's SMART/etc. checks ever Fail in CI for unrelated reasons.

- **`tests/module/pool-lock-readonly-bypass.nix`** -- minimal VM wrapper. Same shape as `tests/module/pool-lock-precedes-state-read.nix`: imports `../../modules/braid`, sets `braid.enable = true; package = braid;`. No initrd fixture, no `virtualisation.emptyDiskImages` -- the test needs no disks because both diagnostics run cleanly against an unconfigured pool (`/etc/braid/config.json` is provided by the NixOS module at `modules/braid/cli.nix:40-41`; the default `--config` path is `/etc/braid/config.json` per `cli/src/config.rs:7` and `cli/src/main.rs:24`).

### Files to modify

- **`flake.nix`** -- register the new test alongside `pool-lock-precedes-state-read` at line 702-706. Same registration pattern:
  ```nix
  pool-lock-readonly-bypass = pkgs.testers.nixosTest (
    import ./tests/module/pool-lock-readonly-bypass.nix {
      braid = linuxCrane.braid-cli-unwrapped;
    }
  );
  ```

- **`docs/principles.md`** -- update Principle 12 (`docs/principles.md:65-67`) to explicitly name `status` and `doctor` as lock-free diagnostics. The current principle paragraph enumerates the locked set and explicitly carves out bare `discover`; the operator-facing exempt commands (`status`, `doctor`) are only implicit and only explicitly named in an implementation plan (`plans/impl/2026-05-19-rust-owned-pool-operation-lock.md:581`), not in the active architecture doc. Add a sentence near the end of the paragraph, e.g.: "Read-only diagnostics `status` and `doctor` never acquire the lock so operators retain a working diagnostic surface during contention; this is what the new test in `tests/module/pool-lock-readonly-bypass.py` pins." The other implicit exempt commands (`idle`, `tui`, `ups`, `help`, internal `scrub-*`) stay implicit -- the architectural concern is the operator-facing pair.

### Files NOT to modify

- `cli/src/main.rs` -- production code is already correct; the test pins existing behavior.
- `plans/impl/2026-05-19-rust-owned-pool-operation-lock.md` -- the migration plan already names the full exempt set at line 581; no change needed.
- `tests/module/pool-lock-precedes-state-read.{py,nix}` -- left alone; its intent is "locked arms acquire before state reads," which is the inverse invariant and deserves its own file.

## Design decisions and rationale

**New file vs. extending `pool-lock-precedes-state-read.py`.** The existing test's preamble pins it to one invariant (acquisition order for locked arms). The new test pins the inverse (non-acquisition for exempt arms). The `tests/module/pool-lock-*.py` family already follows the pattern of one file per invariant (`-contention`, `-replace-contention`, `-discover-contention`, `-lock-contention`, `-enroll-contention`, `-precedes-state-read`); a new `-readonly-bypass` fits that convention. A separate file gives cleaner failure attribution: if `Doctor` ever regresses to acquire the lock, the test that fails is named for the exact invariant that broke.

**Scope: `status` + `doctor` only (with `--json` variants).** Principle 12's full exempt set is `status`, `doctor`, `idle`, `tui`, `ups`, `help`, bare `discover`, and internal `scrub-*`. The finding scopes to `status`/`doctor` because:
- They are the two operator-facing diagnostics during incidents -- the architectural point of the exempt list.
- `idle` and `scrub-*` are systemd-invoked; a regression there is real but lower-priority and unlikely to break a human workflow.
- `tui` requires a TTY (hard to test under held lock from the VM driver).
- `ups` needs UPS configuration outside this test's minimal VM.
- `help` has no I/O -- low regression risk.
- Bare `discover` is already covered in `tests/module/pool-lock-discover-contention.py:54-58`.

Don't gold-plate the test by enumerating the entire exempt list; the goal is to protect the most important invariant, not to mirror the principle's full text.

**Assertion shape: three-layer pin.** The test catches three regression shapes:
1. **Non-blocking fail-fast acquisition** (the copy-paste-from-mutators regression) -- caught by the negative assertion that `"another braid operation is already in progress"` is absent.
2. **Bounded-wait or blocking acquisition** (an `ack`-style or `unlock`-style refactor) -- caught by the combination of (a) holder outliving the command timeout (60s holder vs 5s command timeout) and (b) the `rc != 124` and contention-message assertions covering both timeout-hit and contention-message paths.
3. **Wording-drift false-pass** (a future error wording change makes the negative substring vacuously true) -- caught by the positive output sentinels proving the diagnostic path was reached.

Do not pin `rc == 0`. The structural invariant we are protecting is "did not block on the held lock, reached the diagnostic path," not "doctor's checks all passed." Pinning `rc == 0` would conflate two concerns and create test flakiness if doctor's environmental checks (SMART, etc.) ever return Fail in CI for unrelated reasons. The positive sentinels provide path-reached evidence without the flakiness cost.

**Holder shape: long-lived shared, not per-invocation.** The `with_holder()` pattern from `pool-lock-precedes-state-read.py:23-36` was designed for tests of *acquisition order*: each invocation gets a fresh holder, then the helper waits for holder release in `finally`. For this test, we instead need the *holder to outlive every command's timeout window* so a bounded-wait acquisition path cannot wait it out. The `start_lock_holder` / `stop_lock_holder` shape from `alert-state-lock.py:114-132` (single 60s holder, killed at end) satisfies that requirement and is also faster: ~6s total vs ~80s for four `with_holder` cycles with `hold_secs=15`.

**Stream capture: separate stdout and stderr, not `2>&1`.** The four invocations have two incompatible assertion needs in one helper: parse JSON from stdout (for `--json` variants) and search for the contention substring (which lives on stderr). Merging streams with `2>&1` breaks JSON parsing on the doctor Fail path, where `cmd_doctor` prints JSON to stdout (`cli/src/doctor.rs:1287-1292`) and `main` then prints an "error: doctor failed" line to stderr via `print_cli_error` (`cli/src/main.rs:570-572,1070-1076`); the merged buffer is not parseable JSON. Capturing only stdout would miss the contention message, which goes to stderr via `eprintln!` (`cli/src/main.rs:990` for `PoolLockError::AlreadyHeld`). The proven idiom in the codebase is `2>/tmp/<test>.err` redirecting stderr to a temp file (`tests/cli/braid-doctor-foreign-luks-uuid.py:63`); follow that shape, returning `(rc, stdout, stderr)` from one helper. Search the contention substring across `stdout + stderr` so a future error-helper change that moves the message between streams does not silently break the negative assertion.

## Verification

1. Add the two new files, register the test in `flake.nix`, update `docs/principles.md`.
2. Run `just test-vm pool-lock-readonly-bypass`. Expect: PASS.
3. Confirm the test catches each of the three regression shapes:
   - **Non-blocking fail-fast** (the copy-paste-from-mutators regression). Temporarily prepend `let _pool_guard = acquire_pool_or_exit(&pool_lock);` to the `Doctor` arm at `cli/src/main.rs:562`. Re-run. Expect: FAIL on the "contention message absent" assertion. Revert.
   - **Bounded-wait acquisition** (an `ack`-style 10s wait regression). Temporarily prepend `let _pool_guard = acquire_pool_with_timeout_or_exit(&pool_lock, Duration::from_secs(10));` (using the helper at `cli/src/main.rs:939-948`) to the `Doctor` arm. Re-run. Expect: FAIL on either `rc != 124` (if the timeout fires) or the contention-message assertion (if the bounded-wait returns the AlreadyHeld error). Revert.
   - **Wording-drift false-pass** (positive sentinel must catch). Temporarily edit `cli/src/main.rs:572` to call `std::process::exit(0)` *before* `cmd_doctor` runs (simulating a regression where the command exits before reaching its diagnostic path, with empty output). Re-run. Expect: FAIL on the "`config file` substring present in doctor output" assertion. Revert.
4. Repeat the relevant subset of (3) for `Commands::Status` at `cli/src/main.rs:539`, swapping `cmd_status` for `cmd_doctor` and the `not mounted` / `not_mounted` sentinels.

Step (3) doubles as the TDD validation prescribed by `AGENTS.md` ("Development Approach: TDD with NixOS VM Tests") -- confirm each assertion layer fails for the regression shape it is designed to catch before relying on the test.
