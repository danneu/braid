# plan-the-ideal-fix-swirling-sprout

## Context

A code-review finding (severity Low, category Simplicity) flagged that
`check_braid_online_active_when_mounted` in `cli/src/doctor.rs` carries
its own untyped mountpoint-probe helper (`probe_mountpoint_is_mounted`)
instead of going through `OnlineStateOps::is_mountpoint` -- the typed
seam in `cli/src/online_state.rs` already used by `mark_online` and
`mark_offline`. Two implementations of the same semantics coexist:

- `probe_mountpoint_is_mounted` (`cli/src/doctor.rs:528-535`) returns
  `bool` and folds spawn failure, exit-code 1, exit-code 32, and any
  other non-zero exit code all to `false`.
- `RealOnlineStateOps::is_mountpoint` (`cli/src/online_state.rs:148-163`)
  returns `Result<bool, OnlineError>` mapping exit 0 to `Ok(true)`, exit
  1 to `Ok(false)`, all other exits to `Err::Mountpoint`, and spawn
  failures to `Err::Spawn`.

Recent commit `a2b9445 refactor(cli): dedupe doctor mount probe without
caching the UPS check` deliberately created the doctor-local helper to
keep the safety-critical UPS check re-probing live mount state instead
of consuming a stale cache. That re-probe contract is asserted by
`braid_online_check_reprobes_when_cache_is_stale`
(`cli/src/doctor.rs:4302`). The contract is orthogonal to which helper
does the probing -- it just needs the UPS check to skip
`ctx.mountpoint_is_mounted`.

**The typed seam currently has a wrong util-linux contract.** Pinned
upstream `mountpoint(1)` exits 0 for "is a mountpoint", 32 for "not a
mountpoint", and 1 for invocation/permission/system error
(`reference/util-linux/sys-utils/mountpoint.1.adoc:43-50`;
`MOUNTPOINT_EXIT_NOMNT = 32` at
`reference/util-linux/sys-utils/mountpoint.c:35`). The existing
`RealOnlineStateOps::is_mountpoint` (`cli/src/online_state.rs:154-161`)
treats exit 1 as `Ok(false)` and exit 32 as `Err`. The doctor-local
helper hides that bug by folding all non-zero exits to `false`. If this
plan routed the UPS check through the seam without correcting the
contract, an ordinary UPS-enabled boot with the pool offline (exit 32
-> `Err`) would fire a false-alarm `Fail`, while real probe errors (exit
1) would be silently downgraded to "not mounted". The same correction
benefits the existing `mark_online`/`mark_offline` callers, which today
treat "permission/system error" as "not mounted" too.

The ideal outcome is full dedup *and* correct contract: doctor and
dispatch share one mountpoint-probe seam that matches util-linux;
`probe_mountpoint_is_mounted` is deleted; and the safety-critical UPS
check stops silently swallowing probe failures (per ADR 020 it is the
highest-severity finding and an indeterminate probe should not silently
degrade to Skip).

## Approach

1. **Correct the seam.** In
   `cli/src/online_state.rs:148-163`, change the `match output
   .exit_status` arms to match util-linux:
   - `0 => Ok(true)`
   - `32 => Ok(false)`
   - `1` and every other non-zero exit fall through to
     `Err(OnlineError::Mountpoint { ... })`.
   The `Err::Spawn` arm and the `OnlineError::Mountpoint` shape stay
   as-is. Update the test fixture `mountpoint_fail()` in
   `cli/src/test_fixtures/doctor.rs:173-185` to return `exit_status:
   32` with stderr `"/mnt/storage is not a mountpoint\n"` so all
   existing callers of that helper now exercise the correct contract.
   Update the lone inline mock at `cli/src/doctor.rs:4560-4568`
   (`braid_online_check_skips_when_not_mounted`) to `exit_status: 32`
   for the same reason. Add a small `mod tests` block in
   `cli/src/online_state.rs` (or extend an existing one) covering all
   three mappings: 0 -> `Ok(true)`, 32 -> `Ok(false)`, 1 -> `Err`, plus
   one "other exit" -> `Err`.

2. **Stash the ops on `DoctorContext`.** Add `online_ops:
   RealOnlineStateOps<'a>` to `DoctorContext`. Owning the concrete type
   (not `&'a dyn OnlineStateOps`) lets the existing test helpers and
   constructors construct it from `runner` without changing any of the
   30 test call sites' signatures. If a future doctor test needs a fake
   ops, that's a separate promotion to `Box<dyn OnlineStateOps + 'a>`;
   not needed today because all current doctor tests drive lifecycle
   behavior through `MockRunner` mocks that `RealOnlineStateOps`
   faithfully delegates to.

3. **Delete `probe_mountpoint_is_mounted`.** Route both remaining call
   sites through the (now correct) typed seam:
   - `ensure_mountpoint_is_mounted` (`cli/src/doctor.rs:537-551`) calls
     `ctx.online_ops.is_mountpoint(Path::new(mount_point.as_str()))
     .unwrap_or(false)`. The `unwrap_or(false)` preserves bug-for-bug
     visible behavior for the 4 cached-path callers (`check_*` at
     `cli/src/doctor.rs:565, 600, 680, 716`); they continue to see
     `Option<bool>` and skip with "(pool not mounted ...)" on probe
     failure, exactly as today. (Note: after step 1, an exit-1 probe
     error now produces `Err -> false -> skip` instead of the previous
     `false -> skip`; the user-visible outcome is the same.)
   - `check_braid_online_active_when_mounted`
     (`cli/src/doctor.rs:1013-1078`) matches `ctx.online_ops
     .is_mountpoint(...)` explicitly:
     - `Ok(true)` -> proceed to the `unit_active_state` call (also via
       `ctx.online_ops`, replacing the inline `RealOnlineStateOps::new`
       at `cli/src/doctor.rs:1036`).
     - `Ok(false)` -> `CheckResult::skip(name, "skipped (pool not
       mounted -- braid-online only matters while online)")` (current
       message preserved).
     - `Err(e)` -> `CheckResult::fail(name, "mountpoint probe for {path}
       failed: {e} -- cannot confirm UPS shutdown safety. Re-run `braid
       doctor`.")`. This is the only intentional user-visible behavior
       change in the plan: per ADR 020, the UPS check is safety-critical
       and an indeterminate probe must not silently degrade to Skip.

4. Update the `///` doc comment on
   `check_braid_online_active_when_mounted` (`cli/src/doctor.rs:998-1012`)
   to note that the mountpoint and ActiveState probes both go through
   the shared `OnlineStateOps` seam (same seam as
   `mark_online`/`mark_offline`).

## Files to modify

- `cli/src/online_state.rs`
  - `RealOnlineStateOps::is_mountpoint` (lines 148-163): fix the match
    arms per step 1.
  - `mod tests` (search for the existing `mod tests` block near the
    bottom of the file): add coverage for the three exit-code arms.

- `cli/src/test_fixtures/doctor.rs`
  - `mountpoint_fail()` (lines 173-185): change `exit_status: 1` to
    `exit_status: 32`.

- `tests/cli/braid-doctor.py`
  - Insert a `subtest("mountpoint exit-code behavior lock")` block
    immediately before line 189's "Data profile mismatch -- skip when
    pool not mounted" (i.e. while the pool is still unmounted). See the
    "Live-tool behavior lock" entry in `## Tests` for what it asserts.

- `cli/src/doctor.rs`
  - `DoctorContext` struct (lines 162-172): add `online_ops:
    RealOnlineStateOps<'a>`.
  - `run_doctor` builder (line 1183): initialize `online_ops:
    RealOnlineStateOps::new(runner)`.
  - `for_test_parsed_with_fs` (line 1322): same initializer.
  - `for_test_beep` (line 1344): same initializer.
  - `probe_mountpoint_is_mounted` (lines 528-535): delete.
  - `ensure_mountpoint_is_mounted` (lines 537-551): replace
    `probe_mountpoint_is_mounted` call (line 548) with
    `ctx.online_ops.is_mountpoint(Path::new(mount_point.as_str()))
    .unwrap_or(false)`.
  - `check_braid_online_active_when_mounted` (lines 1013-1078): replace
    the probe call at line 1030 and the inline `RealOnlineStateOps::new`
    at line 1036 with the `match` on `ctx.online_ops.is_mountpoint(...)`
    described in step 3.
  - `braid_online_check_skips_when_not_mounted` (lines 4558-4574):
    update the inline `MountpointCheck` mock to `exit_status: 32`.
  - Update the function's `///` doc comment (lines 998-1012) per step 4.

## Existing functions reused

- `RealOnlineStateOps::new(&dyn CommandRunner)`
  (`cli/src/online_state.rs:127`): the wrapper that doctor will now own
  per-context.
- `OnlineStateOps::is_mountpoint(&Path) -> Result<bool, OnlineError>`
  (`cli/src/online_state.rs:113, 148`): the typed seam, with corrected
  exit-code mapping.
- `OnlineStateOps::unit_active_state(&str) -> Result<UnitActiveState,
  OnlineError>` (`cli/src/online_state.rs:112, 133`): already consumed
  inline at `doctor.rs:1036`; this plan just moves construction up to
  the context.
- `MountPoint::as_str(&self) -> &str` (`cli/src/types.rs:383-390`):
  used in the `Path::new(mp.as_str())` conversion already idiomatic in
  `cli/src/online_state.rs:265`.

## What does NOT change

- The re-probe contract for the UPS check. The new code does not
  consult `ctx.mountpoint_is_mounted`; it calls `ctx.online_ops
  .is_mountpoint(...)` directly every time. The regression test
  `braid_online_check_reprobes_when_cache_is_stale`
  (`cli/src/doctor.rs:4302-4329`) continues to pass unchanged: its
  `MountpointCheck` mock returns exit 0 which `is_mountpoint` maps to
  `Ok(true)`, producing the same `Fail` outcome.
- Cached-path callers' user-visible behavior. The 4 `check_*` sites at
  `cli/src/doctor.rs:565, 600, 680, 716` keep their existing skip
  messages on probe failure, because `ensure_mountpoint_is_mounted`
  still folds Err to false via `.unwrap_or(false)`.
- All 30 test call sites for `parsed_doctor_ctx`/`beep_ctx`/`ups_ctx`.
  They keep their current signatures.
- `mark_online`/`mark_offline` behavior on the now-correct seam. Both
  already handle `Err` by warning and degrading
  (`cli/src/online_state.rs:266-272` and `:340-349`); the only
  difference is that exit-32 stops being mistakenly reported as a
  warning and exit-1 stops being silently treated as "not mounted".

## Tests

- After the `mountpoint_fail()` and inline-mock updates (step 1), all
  existing UPS check tests in `cli/src/doctor.rs:4220-4593` and
  cached-path tests using `mountpoint_fail()` keep passing -- they
  now mock the correct util-linux exit code (32) for "not mounted".
- All existing `mark_online`/`mark_offline` tests in
  `cli/src/online_state.rs` use `RecordingOnlineStateOps` (not
  `MockRunner`); they don't see the exit-code change and need no edits.
- **New unit tests** for `RealOnlineStateOps::is_mountpoint` (per step
  1): one per exit-code arm (0, 32, 1, other -> e.g. 2). Each follows
  the project test-convention triad (Intent / Why / Scenario) per
  `AGENTS.md` -- the Why is "the util-linux mountpoint exit-code
  contract is load-bearing for both UPS shutdown safety and the
  cached-path skip messages; regressing it would silently corrupt
  diagnostics".
- **New doctor test** in `cli/src/doctor.rs`'s `mod tests`:
  `braid_online_check_fails_on_mountpoint_probe_error`. Stages a
  `MountpointCheck` `RawCommandOutput` with `exit_status: 1` (now
  meaning probe error per the corrected contract); asserts
  `check_braid_online_active_when_mounted` returns `Fail` with a
  message mentioning "mountpoint probe" and "UPS shutdown". Preamble
  follows the same Intent / Why / Scenario triad; the Scenario is
  "`mountpoint(1)` returns exit 1 (e.g. permission denied resolving the
  path), and we must not silently downgrade to Skip because that's the
  exact silent-failure mode ADR 020 created this check to catch."

- **Live-tool behavior lock** in `tests/cli/braid-doctor.py`, following
  the project pattern in `docs/testing.md:66-72` ("Live-tool behavior
  locks"; reference example `tests/repro/cryptsetup-close-mounted.py`).
  Mocked unit tests prove the classifier is correct *given* the assumed
  exit codes; the behavior lock proves the assumption itself against the
  pinned util-linux build. Insert the subtest before the existing
  "pool not mounted" block at line 189 so the pool is guaranteed
  offline. The subtest must establish its preconditions explicitly --
  `tests/cli/braid-doctor.nix:36-38` only writes `mount_point =
  "/mnt/storage"` to the config and does not create the directory, and
  `mountpoint.c:190-194` returns `EXIT_FAILURE` (1) when `stat()` fails,
  *before* it can reach the not-a-mountpoint path. So the subtest:
  1. `machine.succeed("mkdir -p /mnt/storage")` to guarantee the path
     exists as a regular directory (not a mountpoint).
  2. Asserts `machine.execute("mountpoint -q /mnt/storage")` returns
     exit 32 (existing directory, not a mountpoint -- the assumption
     powering `Ok(false)`): `status, _ =
     machine.execute("mountpoint -q /mnt/storage"); assert status ==
     32, "mountpoint -q /mnt/storage exit=" + str(status) + ", expected
     32"`. (Plain concatenation; f-strings without placeholders fail
     the build-time linter per `docs/testing.md:58-62`.)
  3. Asserts `machine.execute("mountpoint -q")` returns exit 1 (no
     path given -- the assumption powering the new `Err -> Fail` arm,
     `mountpoint.c:181-184`). Same shape as above.
  Comment the subtest with the standard Intent / Why / Scenario triad;
  the Why is "this is the behavior-lock for the exit-code classifier in
  `cli/src/online_state.rs:148-163` -- a nixpkgs bump that changed
  `mountpoint(1)`'s exit codes would silently misclassify in production
  while every mocked test still passed."

## Verification

1. `just test-rust` -- exercises the affected unit tests:
   `RealOnlineStateOps::is_mountpoint` arms,
   `braid_online_check_reprobes_when_cache_is_stale`,
   `braid_online_check_ok_when_active`,
   `braid_online_check_warn_when_activating`,
   `braid_online_check_fail_when_*`,
   `braid_online_check_skips_when_not_mounted` (now via exit 32),
   `braid_online_check_skips_when_ups_absent`, the cached-path callers
   that consume `mountpoint_fail()`, and the new
   `braid_online_check_fails_on_mountpoint_probe_error`.
2. `just clippy` clean (`cargo clippy --manifest-path cli/Cargo.toml
   --tests` per `justfile:114-116`).
3. `just test-vm braid-doctor` (`flake.nix:265-269`) -- **required
   gate** per `docs/testing.md:66-72`: this is where the live-tool
   behavior lock for `mountpoint(1)` exit codes runs. Without this
   step, a nixpkgs bump that changed the `mountpoint` exit-code
   contract would silently misclassify in production while every
   mocked test still passed.
4. `just test-vm braid-doctor-ups` (`flake.nix:285-289`) -- the
   dedicated UPS doctor VM check exercises the full UPS-enabled boot
   path end-to-end including the new `Err -> Fail` arm via the shared
   doctor renderer.

## Out of scope

- Promoting `online_ops` to `&'a dyn OnlineStateOps` or `Box<dyn
  OnlineStateOps + 'a>` for fake injection. Doctor tests today drive
  lifecycle behavior through `MockRunner` and `RealOnlineStateOps`
  delegates to it transparently. Defer until a doctor test genuinely
  needs to bypass the runner.
- Changing the 4 cached-path callers' user-visible behavior on probe
  failure (currently silent Skip). That's a wider UX change worth its
  own review.
- Re-examining `mark_online`/`mark_offline`'s warn-and-degrade response
  to `Err(OnlineError::Mountpoint)`. The corrected exit-code mapping
  makes that warning fire on real probe errors instead of on "not
  mounted"; whether to escalate is its own design question.
