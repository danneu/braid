# Plan: surface mount-point mkdir failures via the `Filesystem` seam

## Context

Three code paths create the pool mount-point directory immediately before
mounting, but discard the result:

```rust
let _ = std::fs::create_dir_all(mount_point.as_str());
```

- `cli/src/mount.rs` `scan_and_mount` -- the shared tail of `execute_mount_only`
  and `execute_unlock_and_mount`.
- `cli/src/pool.rs` `pool_bootstrap_mount`.
- `cli/src/pool.rs` `pool_bootstrap_mount_raid1`.

When `create_dir_all` fails (a parent component is a non-directory file, the
parent fs is read-only or full, etc.), the error is swallowed and the next
`mount` call fails with a kernel-level `mount: ... No such file or directory` /
`not a directory` message that never names the real cause. On a managed NixOS
install the directory is pre-created by tmpfiles (`modules/braid/storage.nix`)
and sealed immutable (ADR 028), so `create_dir_all` is a no-op there; the bug
only bites misconfigured standalone installs. Low severity, but a real
correctness/UX defect that recurs identically across three sites.

The deeper issue the finding exposes: this raw `std::fs::create_dir_all` is the
one spot in the mount/pool execute layer that escapes braid's dependency-injection
architecture. Because it is un-seamed, it is also untested -- and ~26 existing
tests across `mount.rs`, `pool.rs`, `recover.rs`, and `unlock.rs` silently rely
on the error being swallowed (they pass `/mnt/storage`, which a non-root
`cargo test` cannot create). The `let _ =` masks real-FS failures inside the
test suite.

Outcome: mount-point creation becomes a mockable `Filesystem` operation whose
failure is surfaced fail-closed with a named error before any mount is attempted;
the mount/pool execute layer is uniformly `fs`-aware; tests stay hermetic.

## Design decision: route through the `Filesystem` seam

Mount-point creation is moved behind the existing `probe::Filesystem` trait
(new `create_dir_all` method). `scan_and_mount`, `execute_mount_only`,
`execute_unlock_and_mount`, `pool_bootstrap_mount`, and
`pool_bootstrap_mount_raid1` gain an `fs: &F` parameter, fed by callers that
already hold one.

This reverses the earlier draft's "direct `std::fs`, keep `Filesystem`
read-only" decision. The reversal is grounded in braid's own conventions, not
in test-churn (which AGENTS.md says is not a tiebreaker):

- **ADR 016 already sets the seam rule, and it points here.** For the `braid
  idle` mount probe, ADR 016 chose the `Filesystem` abstraction *because* it is
  a direct syscall "with no fork/exec," explicitly rejecting a subprocess
  fallback. `create_dir_all` is the same shape -- one direct `std::fs` syscall,
  not a `mkdir -p` subprocess -- so it belongs behind `Filesystem`, not as a
  `CmdRequest` through `CommandRunner`.
- **There is no invariant that `Filesystem` is read-only.** Nothing in
  `principles.md` or `decisions/` mandates it. `RealFilesystem` is already
  braid's thin wrapper over direct `std::fs`/`Path` calls; its read-only
  character is incidental. The boundary braid documents is direct-syscall
  (`Filesystem`) vs subprocess (`CommandRunner`), and `create_dir_all` is on
  the syscall side.
- **It removes a real inconsistency.** `validate_pool_topology`,
  `maybe_restore_raid1` (`pool.rs`), `plan_open_pool`, and
  `close_opened_mappers` (`mount.rs`) all already take `fs: &F`. The five
  execute/bootstrap functions are the odd ones out in their own modules.
- **It keeps tests hermetic and fail-closed.** The ~26 tests that reach mount
  already thread a `Filesystem` mock to their entry points, so they need no
  real-FS migration. No defaulted no-op, and no hand-written `Ok(())` in
  read-only doubles either (see section 2): a silent `Ok(())` write -- defaulted
  or per-impl -- is exactly the fail-open seam ADR 016 warns against, so doubles
  for read-only commands `unreachable!` instead.

## Changes

### 1. `Filesystem` trait gains `create_dir_all` (`cli/src/probe.rs`)

No default implementation -- every impl must declare its behavior (a default
`Ok(())` would be a fail-open seam and would mask paths that must never create
a directory):

```rust
pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
    fn is_block_device(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error>;
    /// Create `path` and any missing parents -- the mount/pool execute layer's
    /// one filesystem mutation, kept behind this seam (a direct `std::fs`
    /// syscall, not a subprocess; cf. ADR 016) so the mount path is mockable
    /// and the failure surfaces fail-closed. No default: every impl declares
    /// its behavior, and read-only doubles `unreachable!` here (see section 2)
    /// so an accidental mutation on a read-only path fails loudly.
    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error>;
}
```

`RealFilesystem` (`probe.rs`): `fn create_dir_all(&self, path: &str) -> ... {
std::fs::create_dir_all(path) }` -- behavior identical to today, error now
returnable. This is the only production impl.

### 2. Implement `create_dir_all` on all 24 `Filesystem` doubles

25 `impl Filesystem` blocks exist: `RealFilesystem` (production, above) plus 24
test doubles, all of which must declare `create_dir_all` for the crate to
compile. The rule is **fail-closed**: a double returns `Ok(())` *only* when its
command can actually reach mount-point creation; every other double
`unreachable!`s, so an accidental future mutation on a read-only path fails
loudly instead of passing silently. This is the same anti-fail-open rule that
rules out a trait default, applied per-impl -- a hand-written `Ok(())` in a
read-only double is the same fail-open seam ADR 016 warns against.

The five commands that reach `create_dir_all` are mount, unlock, recover, pool
(bootstrap), and add (via `Add::execute -> pool_bootstrap_mount{,_raid1}`,
`add.rs:1472`/`:1476`).

- **Returns `Ok(())`** -- doubles threaded into those commands' execution tests
  (confirmed by usage: each is passed to `cmd_recover` / `execute_*` /
  `pool_bootstrap_mount*` / `Add::execute`):
  - `test_fixtures/shared.rs` `MockFs` -- mount + unlock tests; also gets the
    failure knob, below.
  - `recover.rs` `MockFs` and `test_fixtures/recover.rs` `RemountFs` -- the
    `cmd_recover` tests (e.g. `recover.rs:8807`, `RemountHarness` at `:7740`).
  - `pool.rs` `RestoreFs` -- the two `pool_bootstrap_mount*` tests.
  - `add.rs` `AddMockFs` / `AddOfflineMockFs` / `AddFullPathFs` /
    `AddMockFsWithSysfs` -- `Add::execute` reaches `pool_bootstrap_mount{,_raid1}`.
    (The function-local `MockFs` in `duplicate_name_rejected` is read-only -- see
    below.)
- **`unreachable!("<Double>: read-only fixture; create_dir_all must never be
  called")`** -- doubles for the read-only commands, which never create a mount
  point; the panic is the fail-closed guard the trait-default rejection demands:
  - `test_fixtures/status.rs` `MockFs` and `test_fixtures/doctor.rs`
    `DoctorMockFs` (`status`/`doctor` are read-only per
    `principles.md#12-one-pool-operation-at-a-time`), `test_fixtures/monitor.rs`
    `MonitorFs` (timer-driven read-only, same principle), `test_fixtures/idle.rs`
    `IdleMockFs` (the read-only mount probe ADR 016 is about),
    `test_fixtures/ack.rs` `AckMountinfoFs` / `OfflineFsThatTouchesSmartd` /
    `MountedFsThatTouchesSmartd` (`ack`).
  - `mount_check.rs` `MockMountInfoFs` and `FailingFs` (mountinfo reads only),
    `tui/probe.rs` `StubFs` (TUI display), `probe.rs` `MockFs` (probe layer),
    `preflight.rs` `MockFs` (preflight checks). Each is private to its own test
    module (no cross-module use), so none can reach `create_dir_all`.
  - `add.rs` `MockFs` -- the function-local double in the
    `duplicate_name_rejected` dry-run test (`add.rs:2568`), which rejects before
    `Add::execute` and never bootstraps, so it never creates a mount point.
  - The existing assert-never-touched doubles keep panicking, now here too:
    `add.rs` `PanicFilesystem`, `replace.rs` `PanicFilesystem`,
    `test_fixtures/ack.rs` `AckPanicFilesystem`.

Add a failure-injection knob to `test_fixtures/shared.rs` `MockFs` (the shared
mount/pool double): an `Option<std::io::ErrorKind>` field defaulting to `None`
(-> `Ok(())`), set via a `with_create_dir_error(kind)` builder, so the new
failure tests reuse the existing fs fixtures.

### 3. Shared helper (`cli/src/util.rs`)

```rust
use crate::probe::Filesystem;
use crate::types::MountPoint;

/// Create the pool mount-point directory through the `Filesystem` seam,
/// surfacing a mkdir failure as a named operator message instead of letting it
/// resurface as a confusing kernel `mount` failure a step later. Idempotent:
/// `create_dir_all` returns Ok when the directory already exists (the NixOS
/// tmpfiles / sealed-dir case) and errors when the directory or a missing
/// parent cannot be created -- for example a path component is a non-directory,
/// or the parent is unwritable or full. (Per std's docs this list is not
/// exhaustive, and some parent directories may have been created before the
/// error.) Returns the message so each caller wraps it in its own error enum.
pub(crate) fn ensure_mount_point_dir<F: Filesystem + ?Sized>(
    fs: &F,
    mount_point: &MountPoint,
) -> Result<(), String> {
    fs.create_dir_all(mount_point.as_str())
        .map_err(|e| format!("could not create mount point {mount_point}: {e}"))
}
```

`MountPoint` implements `Display`, so `{mount_point}` is fine.

### 4. Thread `fs` and adopt the helper

Add `F: Filesystem + ?Sized` + `fs: &F` to the five functions and replace the
`let _ = std::fs::create_dir_all(...)` sites:

- `cli/src/mount.rs` `scan_and_mount`:
  `ensure_mount_point_dir(fs, mount_point).map_err(MountError::Failed)?;`
- `cli/src/pool.rs` `pool_bootstrap_mount`, `pool_bootstrap_mount_raid1`:
  `ensure_mount_point_dir(fs, mount_point).map_err(PoolError::Failed)?;`

`MountError::Failed(String)` / `PoolError::Failed(String)` are the right variant
(not the kernel `mount` command failing, which stays `MountFailed`).

Pass `fs` from callers, all of which already have one:

- `scan_and_mount` <- `execute_mount_only`, `execute_unlock_and_mount` (gain `fs`).
- `execute_mount_only` / `execute_unlock_and_mount` <- `unlock.rs`
  `UnlockPlan::execute`, `recover.rs` `execute_recover_initial_open` (both hold `fs`).
- `pool_bootstrap_mount{,_raid1}` <- `add.rs` `Add::execute` (holds `fs`).

(Verify no other production caller lacks `fs`; the traced callers are these.)

Drop the original finding's "on Err other than the directory already existing"
special-case: `create_dir_all` is already idempotent for an existing directory,
and for an existing path the error it raises is "a non-directory sits here,"
which is exactly what we want to surface.

## Tests

### Unchanged (~23): the payoff of the seam

Tests that reach mount through an entry point that already threads a
`Filesystem` mock need no change -- the mock's `create_dir_all` returns `Ok(())`:

- `recover.rs` -- the 17 `cmd_recover` tests (all wire `BtrfsDeviceScanAll ->
  ok` with `/mnt/storage`).
- `unlock.rs` -- the `cmd_unlock` / `UnlockPlan::execute` mount tests.
- `mount.rs` -- the 4 happy-path tests via `open_and_mount_for_test` (it already
  holds `fs` and now forwards it to `execute_*`).

No `MountpointCheck`/`Mount` reseeding, no temp dirs, no `TempDir` lifetimes.
This is what dissolved the prior round's MountpointCheck and migration-inventory
findings.

### Changed -- add an `&fs` argument only (~10)

Tests that call `execute_mount_only` / `execute_unlock_and_mount` /
`pool_bootstrap_mount{,_raid1}` directly gain an `fs` arg (most already build
one, e.g. `direct_two_disk_fs_with_mappers()`):

- `mount.rs` direct-execute tests: the empty/non-empty plan guards
  (`execute_unlock_and_mount_rejects_empty_plan`,
  `execute_mount_only_rejects_non_empty_plan`) and the cleanup-coverage tests
  (`unlock_failure_after_two_opens_closes_both_after_scoped_forget`,
  `unlock_scan_failure_reports_opened_mappers_for_cleanup`,
  `already_owned_execute_race_is_filtered_from_cleanup_set`,
  `second_open_failure_preserves_error_and_cleans_first_open`,
  `keyfile_post_open_failure_reports_opened_mappers_for_cleanup`).
- `pool.rs`: `pool_bootstrap_mount_runs_mkfs_when_fresh`,
  `pool_bootstrap_mount_raid1_runs_mkfs_when_all_fresh` -- add an `Ok`-returning
  fs (`RestoreFs` or `shared::MockFs`); their `assert_eq!(runner.requests(),
  ...)` is unaffected.

The existing cleanup tests (e.g. `unlock_failure_after_two_opens...`) keep their
wired `Mount` *failure*: with an `Ok`-returning mock fs, `create_dir_all`
succeeds, so the mocked mount failure stays primary and their assertions hold
unchanged. The new post-open failure test (below) covers the *create_dir_all*
failure on that same path.

### New failure tests (TDD red -- added after the seam scaffolds, before the helper is adopted)

These reference the new `fs`-taking signatures and `with_create_dir_error`, so
they only compile once the seam scaffolding (trait method + impls + knob + `fs`
plumbing) lands -- see the Verification ordering below. Behavioral: when the
seam reports a mkdir failure, the named error fires and no mount is attempted.
Wire an fs that fails via `with_create_dir_error(...)`:

- **`mount.rs`, mount-only path (`execute_mount_only`).** `OpenPlan` with empty
  `to_unlock` + `any_open`, a `MockRunner` wiring `BtrfsDeviceScanAll -> ok`
  (runs before the mkdir) but NOT `Mount`, and a `shared::MockFs` with
  `with_create_dir_error(...)`. Assert `Err(MountError::Failed(msg))` with
  `msg.contains("could not create mount point")` and
  `!runner.requests().iter().any(|r| matches!(r, CmdRequest::Mount{..} |
  CmdRequest::MountWithOptions{..}))`. No mappers were opened, so this pins the
  no-open entry point only.
- **`mount.rs`, post-open unlock path (`execute_unlock_and_mount`).** This is
  the F2 gap: the mount-only test proves "no mount requested" but not that a
  mkdir failure *after* LUKS opens preserves the opened-mapper cleanup set.
  Mirror `unlock_failure_after_two_opens_closes_both_after_scoped_forget`
  (`mount.rs:2616`) but swap its wired `Mount` *failure* for a failing
  `create_dir_all`: `direct_two_disk_plan()` (both members to unlock),
  `direct_two_disk_open_runner()` + `BtrfsDeviceScanAll -> ok` + the
  `BtrfsDeviceScanForget` / `CryptsetupClose` cleanup outputs, NO `Mount`; and
  `fs = direct_two_disk_fs_with_mappers().with_create_dir_error(...)`. Assert
  `failure.error` is `MountError::Failed(msg)` with `msg.contains("could not
  create mount point")`, `failure.opened_mappers ==
  [braid-disk1, braid-disk2]` (mkdir fails after both opens, so both are
  cleanup-owned), no `Mount`/`MountWithOptions` in `runner.requests()`, then
  reuse the existing `close_opened_mappers(...)` cleanup-summary assertion.
- **`pool.rs` x2 (`pool_bootstrap_mount{,_raid1}`).** Failing fs; wire
  `MkfsBtrfs` / `MkfsBtrfsRaid1 -> ok`, not `Mount`; assert
  `Err(PoolError::Failed(msg))` with the same substring and no `Mount` in
  `runner.requests()`.

These need no real filesystem -- deterministic and cross-platform. Each gets the
standard Intent / Why it exists / Scenario preamble (file-local style). The
`util.rs` helper and the new trait method get `///` docs.

## Verification

1. **Scaffold the seam (behavior-preserving, compiles clean).** Add the trait
   method + `RealFilesystem` impl + all 24 doubles (`Ok` / `unreachable!` per
   section 2) + the `with_create_dir_error` knob on `shared::MockFs`; thread `fs`
   into the five functions and give the ~10 direct-call tests +
   `open_and_mount_for_test` their `&fs` arg. At the 3 call sites route through
   the seam but **still discard** the result
   (`let _ = fs.create_dir_all(mount_point.as_str());`). `just test-rust` -- all
   existing tests still pass: behavior is unchanged (the error is still
   swallowed, the failure knob is unused so far, and read-only doubles are never
   reached). This is the step that can't come after the tests -- they call these
   signatures.
2. **Red.** Add the four failure tests. They now compile (the signatures and the
   knob exist) and must fail by reaching the unwired `Mount` (MissingMock),
   because the injected mkdir error is still discarded -- not yet failing with
   `could not create mount point`.
3. **Green.** Add the `ensure_mount_point_dir` helper and adopt it at the 3
   sites (`.map_err(MountError::Failed)?` / `.map_err(PoolError::Failed)?`),
   replacing the discard. `just test-rust` -- all pass (4 new; the rest,
   including all 17 recover tests, unchanged). A panic from a read-only-bucket
   double here is a real finding -- a read-only command reached mount-point
   creation -- not a reason to switch that double to `Ok`.
4. `cargo fmt` + `cargo clippy` clean; `scripts/docs/check-output-ascii.py`
   passes (message is ASCII).
5. No NixOS VM test changes: production behavior is unchanged
   (`RealFilesystem::create_dir_all` == `std::fs::create_dir_all`); on managed
   installs the dir exists (tmpfiles + ADR 028 seal), so it stays a no-op `Ok`.

## Rejected alternatives

- **Direct `std::fs` + propagate (earlier draft).** Matches the
  `luks.rs#backup_luks_header_to` precedent and keeps production minimal, but to
  preserve an *undocumented* read-only boundary it pushes ~26 tests onto the
  real filesystem with `TempDir` lifetimes threaded through `RemountHarness` and
  the recover-params builder -- against braid's hermetic-test architecture.
- **`CmdRequest::CreateDir` via `CommandRunner`.** A subprocess-shaped seam for
  a direct syscall; contradicts ADR 016 and would force seeding the new command
  in every mount test (or a `MockRunner` special-case).
- **Default `Ok(())` on the trait method.** A fail-open seam (ADR 016) and pure
  churn-avoidance; rejected in favor of explicit per-impl behavior.

## Files touched

- `cli/src/probe.rs` -- `Filesystem::create_dir_all` + `RealFilesystem` impl.
- `cli/src/mount.rs` -- `fs` into `scan_and_mount`/`execute_mount_only`/
  `execute_unlock_and_mount`; adopt helper; ~7 direct-call tests get `&fs`; add
  2 failure tests (mount-only + post-open unlock cleanup).
- `cli/src/pool.rs` -- `fs` into `pool_bootstrap_mount{,_raid1}` + `RestoreFs`
  impl; 2 tests get `&fs`; add 2 failure tests.
- `cli/src/util.rs` -- `ensure_mount_point_dir` helper.
- `cli/src/test_fixtures/shared.rs` -- `MockFs` `create_dir_all` (`Ok`) +
  `with_create_dir_error`.
- `cli/src/test_fixtures/mount.rs` -- `open_and_mount_for_test` forwards its
  `fs` into `execute_mount_only` / `execute_unlock_and_mount` (currently calls
  them without it).
- All remaining `Filesystem` impls (`add.rs`, `replace.rs`, `preflight.rs`,
  `recover.rs`, `mount_check.rs`, `tui/probe.rs`, `test_fixtures/{recover,idle,
  monitor,status,doctor,ack}.rs`) -- one `create_dir_all` method each (`Ok` for
  the execution doubles, `unreachable!` for the read-only ones; see section 2).
- `cli/src/unlock.rs`, `cli/src/recover.rs`, `cli/src/add.rs` -- pass existing
  `fs` into the now-`fs`-taking callees (production paths).

## Implementation notes

- `fs` is threaded as the second argument (`runner, fs, ...`) of all five
  functions and their call sites, matching the existing
  `plan_open_pool(runner, fs, ...)` / `close_opened_mappers(runner, sleeper,
  fs, ...)` order.
- `config.mount_point()` returns `&MountPoint`, so `scan_and_mount` passes
  `mount_point` to `ensure_mount_point_dir` without an extra `&` (the pool
  bootstrap sites already hold a `&MountPoint`); writing `&mount_point` there
  trips `clippy::needless_borrow`.
- Two cleanup tests beyond the plan's explicit "Changed" list also call
  `execute_unlock_and_mount` and so were given the `&fs` argument to keep the
  crate compiling: `wrong_passphrase_zero_open_cleanup_is_noop` and
  `non_first_disk_verify_rejection_opens_no_mapper`. Both fail before the
  mkdir, so an Ok-returning fs leaves their assertions unchanged.
- `MonitorFs::create_dir_all` panics with `"unexpected monitor fs
  create_dir_all probe: {path}"` to match the panic style of its three sibling
  methods, rather than the generic `unreachable!(...)` the plan prescribed for
  read-only doubles; the fail-loud intent is identical. The two
  `PanicFilesystem` doubles and `AckPanicFilesystem` likewise keep their
  existing panic wording.
