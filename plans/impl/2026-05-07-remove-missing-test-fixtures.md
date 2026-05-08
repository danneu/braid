# remove_missing.rs test fixture migration

## Context

`cli/src/remove_missing.rs` still hosts ~700 lines of inline `*Runner`,
`MockFs*`, and `*_config` scaffolding inside `mod tests`. The
fixture-migration program landed for `replace`, `add`, and `remove`
already extracted that scaffolding into `cli/src/test_fixtures/`
(facade + per-scope submodules + `shared.rs`), with `MockRunner::with_handler`
as the dispatch primitive. `remove_missing` is the next scope.

Goals:

- Move repeated scaffolding (3-disk-1-missing topology, params builder,
  pool.json seeding with pinned devids) into a new
  `cli/src/test_fixtures/remove_missing.rs` submodule, exposed through the
  facade.
- Replace inline `*Runner` structs with broad
  `MockRunner::with_handler` topology installers and per-test
  `with_handler` overrides. Reverse-iteration dispatch lets per-test
  overrides shadow the broad handler without rewriting it.
- Preserve every sharp boundary test verbatim: thread proofs,
  must-not-call assertions, request-ordering proofs, and validation
  precedence proofs do not collapse into the topology.
- Land each sub-commit independently green so any step can be rolled back
  without regressing the test suite.

## Test inventory by behavior family (28 tests)

Everything below is in `mod tests` of `cli/src/remove_missing.rs:580-2535`.

### A. Helper-level: `check_relocation_space` (4 tests)

`check_relocation_space_rejects_insufficient_space` (889),
`_passes_sufficient_space` (939), `_with_missing_id_filters` (977),
`_proceeds_on_command_error` (1029).

Use narrow `EnospcRunner` (only handles `BtrfsDeviceUsageRaw`) or
single-test `FailingRunner`. Direct call to a helper, no plan flow.
**No migration** -- these are sharp helper-level boundaries; the runner
narrowness is the assertion.

### B. Pure-data render tests (6 tests)

`work_plan_steps_show_rebalance_when_clearing_last_missing` (1056),
`_omit_rebalance_with_single_survivor` (1073),
`_omit_rebalance_when_not_last_missing` (1090),
`dry_run_render_targeted_removal_with_balance` (1926),
`remove_missing_confirm_with_rebalance` (1952),
`remove_missing_confirm_single_survivor` (1963),
`resolve_target_fails_when_devid_not_in_membership` (1901),
`plan_preview_renders_warn_above_steps` (2200),
`remove_missing_warn_notes_render_canonical_bracketed_form` (2248).

Hand-built `RemoveMissingWorkPlan` / `RemoveMissingPlan` / notes vec.
No runner, no params, no state paths. **No migration.**

### C. Command-level success (3-device, 1-missing) (3 tests)

`cmd_remove_missing_prunes_acked_stats_for_removed_devid` (1254),
`three_device_pool_soft_rebalance_runs` (1395),
`three_device_two_missing_no_rebalance` (1451).

All use `ThreeDeviceRunner` (`#1112-1220`) +
`three_device_config()` (`#1222-1238`) + `MockFs` (no excl_op). Asserts
include log ordering, post-state membership, acked-stats pruning, and
inhibitor count. **Migrate.**

### D. Worker-thread proof (1 test, hard case)

`device_remove_runs_on_progress_worker_thread` (1505). Uses
`ThreeDeviceRunner::with_thread_recorder`, an inline
`WaitForRemoveDoneSleeper` that yields until `remove_done: AtomicBool`
flips, and `ProgressOutput::Human`. Asserts `BtrfsDeviceRemove` runs on a
different thread than the caller. **Migrate the topology + params; keep
`WaitForRemoveDoneSleeper` inline (sharp seam-coverage fake).**

### E. Failure boundaries (3 tests, hard cases)

`journal_survives_device_remove_failure` (1718),
`journal_survives_soft_balance_failure` (1778),
`enospc_hint_surfaces_through_error_chain` (1859). Use
`FailingRemoveRunner` (`#1642-1701`) and `FailingSoftBalanceRunner`
(`#1563-1639`). Asserts: error wording chain, journal phase markers,
pool.json mutation state per failure point, balance must-not-run after
remove failure. **Migrate by injecting failure via per-test
`runner.with_handler()` over the broad success topology.**

### F. Single-survivor / no-usage-probe (1 test)

`no_usage_probe_for_single_survivor` (835). Uses `RecordingRunner`
(`#665-821`) modeling 2-disk-1-missing. Asserts `BtrfsDeviceUsageRaw`
count is zero. **Migrate by adding a `two_disk_one_missing` variant of
the topology; assert via `MockRunner::requests()` count.**

### G. Validation rejection (4 tests)

`plan_remove_missing_rejects_wrong_missing_id_from_pool_state` (1311) --
inline `WrongMissingIdRunner` proves no `BtrfsDeviceUsageRaw` call.
`plan_remove_missing_preserves_preflight_notes_on_no_missing_devices`
(2370) -- `HealthyPoolRunner` + `MockFsWithExclop("device add")`.
`plan_remove_missing_zero_missing_precedes_live_device_validation`
(2436) -- `HealthyPoolRunner` + `MockFs`.
`plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing`
(2493) -- `NullUnderlyingPoolRunner` + `MockFs`.

`HealthyPoolRunner` and `NullUnderlyingPoolRunner` are narrow
validation-precedence shapes (no balance status, no usage probe). They
exist purely to push `plan_remove_missing` to the validation gate.
**Keep these runners inline.** Only migrate `config_path + state_paths`
to `PoolFixture` and the params struct to `RemoveMissingParamsBuilder`.
Likewise keep `WrongMissingIdRunner` inline -- its sharp value is the
"no `BtrfsDeviceUsageRaw` after rejection" assertion.

### H. Soft-warn ENOSPC surfacing (2 tests, hard cases)

`plan_remove_missing_surfaces_soft_warn_on_command_error` (2067) and
`_on_parse_error` (2146). Use `ThreeDeviceSoftWarnRunner` +
`UsageFailureMode` enum (`#1977-2049`). Asserts plan succeeds, carries
exactly one `PreviewNote::Warn` with canonical body, and steps still
render. **Migrate by injecting the failure via per-test
`runner.with_handler()` on `BtrfsDeviceUsageRaw`. Drop the
`UsageFailureMode` enum -- a 5-line per-test handler reads better than a
shared variant for two tests.**

### I. Preflight busy-op + note preservation (2 tests, hard cases)

`plan_remove_missing_preflight_busy_op_becomes_info_note` (2304) -- 3-disk
1-missing topology + `MockFsWithExclop("device add")`.
`plan_remove_missing_preserves_preflight_notes_on_no_missing_devices`
(2370) -- 2-disk healthy + `MockFsWithExclop("device add")`.

**Migrate by deleting `MockFsWithExclop` and using
`MockFs::storage(vec![]).with_excl_op("device add\n")` from
`shared.rs`.** The 2-disk healthy test reuses the local
`HealthyPoolRunner` (kept inline per group G) plus the new
`two_disk_devids_pinned` `PoolFixture`.

## Migration scope

### New module: `cli/src/test_fixtures/remove_missing.rs`

```
pub(crate) struct RemoveMissingPool { ... }
pub(crate) struct RemoveMissingParamsBuilder<'a> { ... }
impl PoolFixture { fn three_disk_devids_pinned() / fn two_disk_devids_pinned() }
const THREE_DISK_PRE_SHOW: &str;     // 3 disks, devid 3 MISSING
const THREE_DISK_POST_SHOW: &str;    // 2 disks, no missing
const TWO_DISK_PRE_SHOW: &str;       // 2 disks, devid 2 MISSING
const TWO_DISK_POST_SHOW: &str;      // 1 disk only
const USAGE_RAW_THREE_DISK_ONE_MISSING: &str;  // 3 entries incl. <missing>
const USAGE_RAW_TWO_DISK_ONE_MISSING: &str;    // 2 entries incl. <missing>
```

### `RemoveMissingPool` topology (state-flipping)

Modeled on `ReplacementPool`: holds an internal pre/post pair plus
`still_degraded_after: bool`, and `install(runner)` registers one broad
handler that flips response shape on
`remove_done: Arc<AtomicBool>`.

```rust
pub(crate) struct RemoveMissingPool {
    pre_show: &'static str,
    post_show: &'static str,
    usage_raw: &'static str,
    still_degraded_after: bool,
}

impl RemoveMissingPool {
    pub(crate) fn three_disk_one_missing() -> Self;       // default: post = 2 healthy
    pub(crate) fn two_disk_one_missing() -> Self;         // single-survivor 2->1
    pub(crate) fn still_degraded_after(self, b: bool) -> Self;  // post still shows MISSING

    /// Returns (runner, remove_done) so per-test handlers can also flip
    /// the same flag if they shadow `BtrfsDeviceRemove`.
    pub(crate) fn install(self, runner: MockRunner) -> (MockRunner, Arc<AtomicBool>);
}
```

The broad handler covers `BtrfsFilesystemShow` (state-flipped),
`CryptsetupStatus`, `CryptsetupLuksUuid`, `BtrfsBalanceStatus`,
`BtrfsDeviceUsageRaw`, `BtrfsDeviceRemove` (sets `remove_done` via
`SeqCst`), `BtrfsBalanceRaid1Soft`. It returns `None` for everything
else so per-test handlers can extend the surface.

### `RemoveMissingParamsBuilder`

Mirrors `RemoveParamsBuilder`. Default shape: `dry_run=false, yes=true,
progress=Off, sleeper=&NoopSleeper, missing_id=3`. Setters:
`missing_id(u64)`, `dry_run(bool)`, `yes(bool)`, `progress(...)`,
`sleeper(&dyn Sleeper)`. Build to `RemoveMissingParams<'a>`. Factory:
`PoolFixture::remove_missing_params(&self) -> RemoveMissingParamsBuilder<'_>`.
The sleeper-override seam covers the worker-thread test only; everything
else takes the default.

### `PoolFixture` ctors (scope-local)

Both pin devids matching the suffix because `--missing-id N` resolves
through the membership map; the existing shared `two_disk_healthy()` and
`one_live_one_missing()` do not pin both disks.

```rust
impl PoolFixture {
    pub(crate) fn three_disk_devids_pinned() -> Self;  // disk1=1, disk2=2, disk3=3
    pub(crate) fn two_disk_devids_pinned() -> Self;    // disk1=1, disk2=2
}
```

If a future scope (e.g. `recover`) also needs pinned devids, promote to
`shared.rs`. Until then, keep scope-local.

### Reused from shared.rs (no changes)

- `mock_ok(cmd, stdout)` -- replaces local `mock_out(..., 0)` calls.
  Where `exit_status != 0` (failure injection), build `RawCommandOutput`
  literal in the per-test handler.
- `MockFs::storage(vec![])` -- replaces local `struct MockFs;`.
- `MockFs::storage(vec![]).with_excl_op("device add\n")` -- replaces
  local `MockFsWithExclop("device add")`.
- `PoolFixture` `_state_tmp` / `_config_tmp` RAII guards -- replaces
  manual `tempfile::tempdir()` + `std::fs::write(config.json, ...)`
  scaffolding.

### Stays inline (sharp local fakes)

- `WaitForRemoveDoneSleeper` (worker-thread test only) -- proves the
  device-remove call lands on the progress helper thread; the
  busy-yield-on-`AtomicBool` is the assertion.
- `WrongMissingIdRunner` (test 10) -- proves no `BtrfsDeviceUsageRaw`
  before validation; collapsing into a topology that handles
  `BtrfsDeviceUsageRaw` would mask future regressions even if the test
  asserts via `requests()`.
- `HealthyPoolRunner` (tests 26, 27) -- narrow validation-gate shape.
- `NullUnderlyingPoolRunner` (test 28) -- specific `device: (null)`
  cryptsetup status; sharp coverage of the hot-unplug branch.
- `EnospcRunner` (tests 2-4) and inline `FailingRunner` (test 5) --
  helper-level boundary fakes; never grow.
- `acked_disk(missing_acked, read_io_errs)` (helper, used by test 9
  only) -- module-private; no fixture promotion.
- `mp() -> MountPoint("/mnt/storage".into())` -- one-liner, used by
  the unmigrated render-only tests; leave alone.

### Deleted at the end of migration

- `mock_out(cmd, stdout, exit_status)` (`#676-683`) -- replaced by
  `mock_ok` plus per-call literal `RawCommandOutput` for the
  exit-nonzero failure injections.
- `test_paths(disks)` (`#621-632`) -- subsumed by the two
  `PoolFixture` ctors above. Tests in groups G keep their inline
  state setup but switch to `PoolFixture::two_disk_devids_pinned()`.
- `three_device_config()` (`#1222-1238`) -- subsumed by
  `PoolFixture::three_disk_devids_pinned()`.
- `RecordingRunner` (`#665-821`), `ThreeDeviceRunner` (`#1112-1220`),
  `FailingRemoveRunner` (`#1642-1701`),
  `FailingSoftBalanceRunner` (`#1563-1639`),
  `ThreeDeviceSoftWarnRunner` + `UsageFailureMode` (`#1977-2049`),
  `MockFsWithExclop` (`#2267-2291`) -- all replaced by topology +
  per-test handler.
- The `struct MockFs;` local (`#593-617`) -- replaced by
  `shared::MockFs::storage(vec![])`.

## Module skeleton

`cli/src/test_fixtures/remove_missing.rs` (new file):

```rust
//! Remove-missing fixtures: `RemoveMissingPool`,
//! `RemoveMissingParamsBuilder`, and remove-missing-only `PoolFixture`
//! ctors with pinned devids.

use super::shared::{PoolFixture, mock_ok};
use crate::cmd::{CmdRequest, MockRunner};
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::progress::{self, ProgressOutput};
use crate::remove_missing::RemoveMissingParams;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const THREE_DISK_PRE_SHOW: &str = /* devid 1, 2, 3 MISSING */;
const THREE_DISK_POST_SHOW: &str = /* devid 1, 2 */;
const TWO_DISK_PRE_SHOW: &str = /* devid 1, 2 MISSING */;
const TWO_DISK_POST_SHOW: &str = /* devid 1 only */;
const USAGE_RAW_THREE_DISK_ONE_MISSING: &str = /* 3 disks incl. <missing> */;
const USAGE_RAW_TWO_DISK_ONE_MISSING: &str = /* 2 disks incl. <missing> */;
```

`cli/src/test_fixtures.rs` adds:

```rust
mod remove_missing;
#[allow(unused_imports)]
pub(crate) use remove_missing::{RemoveMissingPool, RemoveMissingParamsBuilder};
```

The `#[allow(unused_imports)]` mirrors the existing pattern on the
`remove::{...}` re-export at `cli/src/test_fixtures.rs:37` and keeps C1
warning-clean before any test consumes the new symbols. It is removed
in C8 once C2-C7 have wired up consumers (parallels the same dead-code
allow lifecycle on the remove-scope re-export).

The `mock_ok` / `MockFs` / `PoolFixture` re-exports already cover what
remove-missing tests will import.

## Conventions copied from existing scopes

- Topology installer named after the pool shape it models
  (`RemoveMissingPool`), with `with_*` builder setters and a single
  `install(runner) -> (MockRunner, Arc<AtomicBool>)` (replace returned
  `MockRunner` only; remove-missing exposes `remove_done` so per-test
  handlers can also gate on it).
- Constants are module-private; export accessor fns
  (`pub(crate) fn three_disk_pre_show() -> &'static str`, etc.) only
  if a per-test handler needs to reuse the canonical body in an
  override. Most overrides are shape-specific failures, not body
  re-use, so I expect zero accessors at first.
- `PoolFixture` ctors return owned tempdirs guarded as `_state_tmp` /
  `_config_tmp`; tests bind the fixture to a local
  `let pool = PoolFixture::three_disk_devids_pinned();` and call
  `pool.remove_missing_params().missing_id(3).build()`.
- Topology handler returns `Option<Result<...>>`: `Some(Ok(...))` to
  fulfil, `None` to defer to the next handler in reverse order.
- Per-test override: `let runner = ...install... ; let runner =
  runner.with_handler(move |req| match req { ... });`. Because
  `MockRunner` iterates handlers in reverse order
  (`cmd.rs:1031-1036`), the override wins for the requests it covers.
- `#[allow(dead_code)]` is acceptable on builder setters and ctors
  added in C1 that no test consumes until later sub-commits.

## Sub-commit plan

Each row is independently green:
`cargo check --manifest-path cli/Cargo.toml --tests` and
`cargo test --manifest-path cli/Cargo.toml --lib remove_missing::tests`
both pass before merging that commit.

| # | Commit subject (lowercase first word) | Scope |
|---|---|---|
| C1 | `refactor(test): scaffold remove-missing test_fixtures module` | New `cli/src/test_fixtures/remove_missing.rs` with `RemoveMissingPool`, `RemoveMissingParamsBuilder`, `PoolFixture::{three,two}_disk_devids_pinned`, and constants. Add `mod remove_missing;` and a `#[allow(unused_imports)] pub(crate) use remove_missing::{...};` re-export to `cli/src/test_fixtures.rs` (mirroring the existing `remove::{...}` re-export at line 37). No tests change. `#[allow(dead_code)]` on every public item that has no consumer yet; the `unused_imports` allow on the facade re-export covers the dual case where the symbol is exported but no test imports it from the facade yet. C8 strips both allows. |
| C2 | `refactor(remove-missing): migrate soft-warn tests to topology` | Migrate `plan_remove_missing_surfaces_soft_warn_on_command_error` (2067) and `_on_parse_error` (2146). Use `RemoveMissingPool::three_disk_one_missing().install(...)`, then per-test `runner.with_handler` on `BtrfsDeviceUsageRaw` to inject `Err(CmdError::MissingMock)` or `Ok(exit_status=1)`. Validates per-test override shadowing the broad handler. |
| C3 | `refactor(remove-missing): migrate worker-thread proof to topology` | Migrate `device_remove_runs_on_progress_worker_thread` (1505). Topology installs the broad handler; per-test handler also matches `BtrfsDeviceRemove` to record `std::thread::current().id()` and set `remove_done`. The local `WaitForRemoveDoneSleeper` stays inline. Builder uses `.sleeper(&waiter)` and `.progress(ProgressOutput::Human)`. Validates the topology composes cleanly with a per-test handler that ALSO returns `Some(Ok(...))` for an already-handled request and the `remove_done` flag still flips. |
| C4 | `refactor(remove-missing): migrate failure-boundary tests` | Migrate `journal_survives_device_remove_failure` (1718), `journal_survives_soft_balance_failure` (1778), `enospc_hint_surfaces_through_error_chain` (1859). Each uses the broad topology + per-test `with_handler` returning `Ok(RawCommandOutput { exit_status: 1, stderr: ..., ... })` for `BtrfsDeviceRemove` (test 14) or `BtrfsBalanceRaid1Soft` (tests 15, 16). The journal/membership/inhibitor assertions are untouched. |
| C5 | `refactor(remove-missing): migrate busy-op + note preservation tests` | Migrate `plan_remove_missing_preflight_busy_op_becomes_info_note` (2304) and `plan_remove_missing_preserves_preflight_notes_on_no_missing_devices` (2370). Replace `MockFsWithExclop` with `MockFs::storage(vec![]).with_excl_op("device add\n")`. Test 25 uses `RemoveMissingPool::three_disk_one_missing()` + new fixture and takes the builder default `missing_id=3`. Test 26 keeps the local `HealthyPoolRunner` but switches to `PoolFixture::two_disk_devids_pinned()` + `RemoveMissingParamsBuilder` with `.missing_id(999)` and `.dry_run(true)` -- the 999 sentinel is what triggers the no-missing validation under test. |
| C6 | `refactor(remove-missing): migrate command-level success tests` | Migrate `cmd_remove_missing_prunes_acked_stats_for_removed_devid` (1254), `three_device_pool_soft_rebalance_runs` (1395), `three_device_two_missing_no_rebalance` (1451). Test 12 uses `.still_degraded_after(true)`. Acked-stats setup and post-state assertions stay. Use `MockRunner::requests()` for the remove-before-balance ordering proof. |
| C7 | `refactor(remove-missing): migrate single-survivor + validation-rejection tests` | Migrate `no_usage_probe_for_single_survivor` (835) using `RemoveMissingPool::two_disk_one_missing()` with `.missing_id(2)` (the surviving disk1 is devid 1; the missing target is devid 2); assert via `runner.requests().iter().filter(...).count() == 0`. For tests 10 (`plan_remove_missing_rejects_wrong_missing_id_from_pool_state`), 27 (`_zero_missing_precedes_live_device_validation`), and 28 (`_null_underlying_empty_missing_devids_not_no_missing`), keep the local `WrongMissingIdRunner` / `HealthyPoolRunner` / `NullUnderlyingPoolRunner` runners inline; only migrate the temp-paths + params construction to `PoolFixture` + `RemoveMissingParamsBuilder`. Each test's `missing_id` is the assertion subject and must be passed explicitly: test 10 uses `.missing_id(99).dry_run(true)`, test 27 uses `.missing_id(1).dry_run(true)`, test 28 uses `.missing_id(2).dry_run(true)`. The builder default of 3 would silently change every assertion's error wording -- the migration is wrong if any of these overrides is dropped. |
| C8 | `refactor(remove-missing): drop dead inline scaffolding` | Delete `RecordingRunner`, `ThreeDeviceRunner`, `FailingRemoveRunner`, `FailingSoftBalanceRunner`, `ThreeDeviceSoftWarnRunner`, `UsageFailureMode`, `MockFsWithExclop`, `mock_out`, `test_paths`, `three_device_config`, and the local `struct MockFs;`. Remove `#[allow(dead_code)]` markers from items that now have consumers in `test_fixtures/remove_missing.rs`. Confirm `cargo check --tests` is clean. |
| C9 | `docs(plans): promote remove-missing fixture migration plan` | Move `plans/wip/here-is-a-prompt-merry-moon.md` to `plans/impl/<YYYY-MM-DD>-remove-missing-test-fixtures.md`. |

C1 is intentionally a no-op for tests; C2-C5 are the four hard cases
that validate the design (override shadowing, thread + sleeper seams,
exit-nonzero failure injection, fs-mock variant); C6-C7 are bulk and
should be near-mechanical once C2-C5 land; C8 is the cleanup pass.

## Verification commands

Run after each sub-commit (and a final pass before C9):

```sh
# Fast iteration loop:
cargo check --manifest-path cli/Cargo.toml --tests
cargo test  --manifest-path cli/Cargo.toml --lib remove_missing::tests

# Hard-case spot-checks (run during C2-C5 development):
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::plan_remove_missing_surfaces_soft_warn_on_command_error
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::plan_remove_missing_surfaces_soft_warn_on_parse_error
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::device_remove_runs_on_progress_worker_thread
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::journal_survives_device_remove_failure
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::journal_survives_soft_balance_failure
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::enospc_hint_surfaces_through_error_chain
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::plan_remove_missing_preflight_busy_op_becomes_info_note
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::plan_remove_missing_preserves_preflight_notes_on_no_missing_devices
cargo test --manifest-path cli/Cargo.toml --lib \
    remove_missing::tests::no_usage_probe_for_single_survivor

# At every sub-commit boundary:
just test-rust
```

`just test-rust` runs the full Rust suite (CLI + parsers); each
sub-commit must keep it green.

## Risk notes

- **Topology rigidity for the post-remove SHOW**. State-flipping via
  `remove_done: AtomicBool` is more brittle than the success-only
  `RemovalPool`. If a test handler also matches `BtrfsDeviceRemove` and
  forgets to call `remove_done.store(true, SeqCst)`, the post-state
  branch never fires and downstream SHOW probes still report the
  missing devid. Mitigation: in C3 (worker-thread test), the override
  must call `remove_done.store(...)` because the topology's
  `BtrfsDeviceRemove` branch is shadowed by the override. Document the
  contract in the topology doc-comment and keep the failing-runner
  overrides in C4 from matching `BtrfsDeviceRemove` unless they need to
  fail it.

- **Hidden no-probe / no-call assertions**. Tests 1, 10, and 12 all
  encode "this command must NOT run". A topology that defaults to
  serving every probe shape can silently pass a future regression that
  starts emitting an extra probe. Mitigation: where the assertion is
  the boundary (test 1 -- no `BtrfsDeviceUsageRaw` call), use
  `MockRunner::requests()` for an explicit count assertion rather than
  trusting the topology to refuse. For test 10, keep
  `WrongMissingIdRunner` inline -- the runner deliberately does NOT
  serve `BtrfsDeviceUsageRaw`, so a regression that adds an extra
  probe surfaces as a `MissingMock` panic instead of a quiet pass.

- **Request-order assertions** (test 11: remove must precede balance).
  `MockRunner::requests()` is append-only; reverse-iteration
  dispatch does not change the order requests are logged
  (`cmd.rs:1172-1175, 1186-1189`). Mitigation: tests use
  `runner.requests().iter().position(...)` to compare indices, same as
  the current log-based approach.

- **Progress worker-thread coverage** (test 13). The thread-id
  assertion only catches a regression if `BtrfsDeviceRemove` is
  actually dispatched on the helper thread. The override must execute
  on whatever thread the runner is invoked from -- per-test handlers
  run on the same thread as `MockRunner::run`, so the assertion
  remains valid as long as `pool_remove_device_using` continues to
  spawn its progress worker. Mitigation: keep `WaitForRemoveDoneSleeper`
  inline; do not push it into the fixture module, because its
  busy-yield-on-`AtomicBool` is the seam-coverage proof.

- **Soft-warn body drift**. Tests 21 and 22 assert canonical prefix
  ("ENOSPC pre-flight check failed: ") and suffix ("; proceeding
  anyway"). Migration must NOT touch those substrings. Per-test
  handler returns the failure shape; the body assertion still reads
  through `plan.notes[0]`.

- **Inhibitor count assertions**. Tests 1, 9, 11, 12, 14, 15 all
  assert `inhibitor.acquire_count() == 1`. The fixture's
  `RecordingInhibitor` is the same type already used by replace, add,
  and remove fixtures, so this is no risk -- listing it for
  completeness.

- **`PoolFixture::two_disk_devids_pinned` overlap**. Tests 26, 27, 28
  could in principle reuse the existing
  `PoolFixture::one_live_one_missing` (which pins disk2 to devid 2 but
  leaves disk1 unpinned). They do not, because the validation
  precedence check in test 27 (`--missing-id 1`) needs the live-device
  branch to be reachable in principle, which in turn needs disk1 to
  carry devid 1 in the membership. Mitigation: scope the new ctor to
  remove-missing rather than mutating the shared
  `one_live_one_missing` (which would silently widen the contract for
  replace tests).
