# Lock test-fixture migration

## Context

`cli/src/lock.rs` is the next migration in the test-fixture series that has
already landed `mount`, `unlock`, `recover`, `status`, `replace`, `add`,
`remove`, `remove_missing`, `doctor`, and `enroll_key_file` modules under
`cli/src/test_fixtures/`. Lock is a mutating command with **41 tests** in a
**~2,000-line** `mod tests` block (lock.rs:559–2556). Roughly 10% of those
lines are local scaffolding (RecordingRunner, MockFs, NoopSleeper, ok_raw /
err_raw, with_fsid_probe_mocks, test_config / test_membership,
mounted_runner / umount_failed_runner, dry-run assertion helpers); the rest
is test bodies. The goal is to extract the repeated scaffolding into a
focused `test_fixtures::lock` module while preserving the load-bearing
contracts that several lock tests intentionally depend on:

- `MissingMock` panics that prove a command was NOT issued
- `RecordingRunner` request order, retry counts, and pool-scoped forget args
- exact dry-run output snippets and step ordering
- error-priority semantics (umount-error wins vs. mapper-error wins)
- busy-hint gating that depends on diagnostic-segment text, not full stderr
- `CLOSE_RETRY_DELAY` real wall-clock sleeping in the public-API smoke test

The migration must not introduce broad `MockRunner::with_handler` patterns
that silently resolve probes lock tests deliberately omit.

## Current-state inventory

### Local helpers in `cli/src/lock.rs::tests`

| Helper | Lines | Role |
|---|---|---|
| `RecordingRunner` (struct + impl) | 576–634 | Wraps `MockRunner`; records `CryptsetupClose` and `BtrfsDeviceScanForget` calls; per-mapper response queues for retry tests. |
| `MockFs` (struct + impl) | 636–697 | `Filesystem` mock with paths + `with_exclop` + `with_mountinfo`; `list_dir` derives entries from paths by prefix-stripping. |
| `NoopSleeper` | 702–705 | `Sleeper` impl with empty `sleep`. Identical body to `mount::NoopSleeper`. |
| `ok_raw` / `err_raw` | 707–723 | `RawCommandOutput` builders. Identical bodies to `mount::ok_raw` / `mount::err_raw`. |
| `with_fsid_probe_mocks` | 728–744 | Adds canonical `BtrfsFilesystemShow` for `/mnt/storage` returning two-disk pool with fixed UUID. |
| `test_config` | 746–748 | `Config::new(MountPoint("/mnt/storage".into())).unwrap()`. **Identical to `mount::test_config`.** |
| `test_membership` | 750–761 | Two-disk membership with keys `aaa`/`bbb` at `/dev/disk/by-id/a` / `/dev/disk/by-id/b`. **Distinct from `mount::two_disk_membership`** (which uses `disk1`/`disk2` at `virtio-disk{1,2}`). |
| `mounted_runner` | 772–794 | Composite preflight runner (mountpoint=ok + fsid probe + umount=ok + forget=ok for both pool mappers). |
| `umount_failed_runner` | 1505–1518 | Same as mounted_runner but with umount=ebusy and no forget mock (gated on umount success). |
| `forget_step_devices` | 2211–2222 | Extracts the device list from the single forget step in a step list. Panics on zero or multiple. |
| `count_forget_steps` | 2224–2230 | Counts forget steps in a compiled plan. |
| `FailListDirFs` (inline in 2 tests) | 1250–1276, 1318–1335 | One-off `Filesystem` impl whose `list_dir` returns `PermissionDenied`. |
| `RecordingSleeper` (inline in 1 test) | 2431–2436 | `Sleeper` impl that records every `Duration` passed to `sleep`. Used only by the helper-level retry-delay test. |

### 41 tests, grouped by behavior family

| Group | # | Tests |
|---|---:|---|
| A. Happy / partial / forget | 6 | `lock_happy_path_unmounts_and_closes`, `execute_does_not_close_membership_mapper_absent_from_plan`, `lock_already_locked`, `lock_partial_state`, `lock_adds_forget_after_umount`, `lock_forget_failure_is_nonfatal` |
| B. Umount-failure variants | 4 | `lock_umount_busy_fails`, `lock_umount_busy_includes_hint`, `lock_umount_non_busy_omits_hint`, `lock_umount_path_containing_busy_phrase_omits_hint` |
| C. Orphan + dry-run preview | 6 | `lock_closes_orphaned_mapper`, `lock_orphan_scan_failure_is_nonfatal`, `dry_run_preview_warns_when_list_dir_fails`, `dry_run_preview_warns_per_orphan_mapper`, `dry_run_preview_mounted_happy_path`, `dry_run_preview_nothing_to_do` |
| D. Error-precedence | 7 | `lock_umount_fails_but_mappers_close_successfully`, `lock_umount_fails_busy_mapper_is_warning`, `lock_umount_fails_unexpected_mapper_error_is_fatal`, `lock_mapper_close_fatal_when_umount_succeeded`, `lock_umount_fails_orphan_busy_is_warning`, `lock_umount_fails_orphan_unexpected_error_is_fatal`, `lock_orphan_close_failure_is_fatal` |
| E. RecordingRunner / retry | 5 | `lock_continues_closing_after_mapper_error`, `lock_collects_first_mapper_error`, `lock_retries_busy_close_then_succeeds`, `lock_mapper_close_exit5_is_busy_regardless_of_wording`, `lock_busy_close_exhausts_retries_preserves_stderr_contract` |
| F. Refusal + dry-run render | 9 | `lock_refuses_when_exclusive_op_active`, `lock_refuses_when_balance_paused`, `lock_rejects_mounted_but_not_btrfs`, `dry_run_render_lock_mounted_2_disks`, `dry_run_lock_not_mounted_1_open`, `dry_run_lock_nothing_to_do`, `dry_run_lock_forget_step_lists_scoped_devices`, `dry_run_lock_forget_step_includes_orphans`, `dry_run_lock_forget_step_omitted_when_no_mappers` |
| G. Forget-recording | 2 | `lock_forget_is_pool_scoped`, `lock_forget_includes_orphan_mappers` |
| H. Untouched (real-clock contracts) | 2 | `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts` (RecordingSleeper, helper-level), `cmd_lock_wrapper_uses_real_sleeper` (proves `RealSleeper` is wired in the public API path) |

## Recommended approach

Lock ships **flat** helpers (no topology installer, no params builder), like
`mount`, `unlock`, `enroll_key_file`, and `status`. Reasons specific to lock:

- Several tests rely on missing-mock `MissingMock` panics (e.g.
  `execute_does_not_close_membership_mapper_absent_from_plan`,
  `lock_adds_forget_after_umount`).
- `RecordingRunner` is a specialized runner whose API (`close_calls`,
  `forget_calls`, `with_close_sequence`) is lock-unique; it will be the
  centerpiece of the new module rather than a generic shared piece.
- Lock has no params struct -- entry points (`cmd_lock_impl`, `plan_lock`,
  `LockPlan::execute`) take positional args.

### A. New module `cli/src/test_fixtures/lock.rs`

```rust
//! Lock-scope fixtures: cross-test scaffolding for `cli/src/lock.rs`'s
//! `mod tests`.
//!
//! Lock is a mutating command whose tests rely on:
//!   * exact `MissingMock` contracts (proves a command was NOT issued)
//!   * `RecordingRunner` ordering / retry counts / pool-scoped forget args
//!   * exact dry-run output snippets and step ordering
//!   * error-priority semantics (umount vs mapper)
//!   * busy-hint gating on diagnostic-segment text, not full stderr
//!
//! Ships flat (no topology installer, no params builder) for the same reason
//! `mount`, `unlock`, and `enroll_key_file` do: a broad handler would
//! silently resolve probes that tests deliberately omit.
//!
//! Naming: free-function helpers that could collide on the facade carry
//! a `lock_` prefix (`lock_test_membership`, `lock_mounted_runner`,
//! `lock_with_fsid_probe_mocks`, ...). Types are declared here without a
//! prefix but re-exported by the facade under a `Lock`-prefixed alias
//! whenever a same-named local in `lock.rs::tests` would otherwise shadow
//! the import during the staged migration (`RecordingRunner as
//! LockRecordingRunner`, `mount::NoopSleeper as LockNoopSleeper`).

use super::shared;
use crate::cmd::{CmdRequest, CommandRunner, MockRunner, RawCommandOutput, Step};
use crate::membership::{DiskMember, PoolMembership};
use crate::types::{ByIdPath, MountPoint};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// RecordingRunner
// ---------------------------------------------------------------------------

/// Runner that delegates to `MockRunner` but records every
/// `CryptsetupClose` and `BtrfsDeviceScanForget` request, and optionally
/// drains a per-mapper queue of close responses (used to model transient
/// busy-then-success retry sequences).
///
/// Test-facing name on the facade is `LockRecordingRunner`: the type is
/// declared here as `RecordingRunner` and the facade re-exports it via
/// `pub(crate) use lock::RecordingRunner as LockRecordingRunner;` to
/// avoid colliding with the same-named local struct in `lock.rs::tests`
/// during the staged migration. The aliased name stays after C8 deletes
/// the local, matching the `LockNoopSleeper` convention.
pub(crate) struct RecordingRunner { /* ... */ }

impl RecordingRunner {
    pub(crate) fn new(inner: MockRunner) -> Self;
    pub(crate) fn with_close_sequence(self, mapper: &str, outputs: Vec<RawCommandOutput>) -> Self;
    pub(crate) fn close_calls(&self) -> Vec<String>;
    pub(crate) fn forget_calls(&self) -> Vec<Vec<String>>;
}

impl CommandRunner for RecordingRunner { /* delegates with recording */ }

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Wraps `shared::MockFs::storage` with the lock-test-canonical default
/// (mounted at /mnt/storage, no in-flight excl op). **Auto-derives the
/// `/dev/mapper` listing from `paths`** by stripping the `/dev/mapper/`
/// prefix from any path beneath that directory; preserves the previous
/// local mock's behavior at `lock.rs:683-696` so orphan-scan tests do not
/// have to redundantly call `.with_dev_mapper(...)`. Tests that want a
/// scan failure chain `.with_dev_mapper_error()`; tests that want a
/// listing distinct from `paths` chain `.with_dev_mapper(&[...])`.
pub(crate) fn lock_fs(paths: &[&str]) -> shared::MockFs;

// ---------------------------------------------------------------------------
// Mount re-exports under lock-prefixed aliases
// ---------------------------------------------------------------------------

// Aliased so the lock-side import path does not collide with the same-named
// locals in `lock.rs::tests` during the staged migration (C2-C7 import the
// alias while the local still exists for unmigrated tests). Matches the
// `err_raw as enroll_err_raw` precedent at `test_fixtures/enroll_key_file.rs:58`.
// The aliased names stay after C8 deletes the locals, so test imports do
// not churn again.
pub(crate) use super::mount::{
    NoopSleeper as LockNoopSleeper,
    err_raw as lock_err_raw,
    ok_raw as lock_ok_raw,
    test_config as lock_test_config,
};

// ---------------------------------------------------------------------------
// Pool fixture (membership)
// ---------------------------------------------------------------------------

/// Canonical 2-disk membership keyed by short names "aaa"/"bbb". Distinct
/// from `mount::two_disk_membership` which uses "disk1"/"disk2"; the short
/// names match every assertion against `/dev/mapper/braid-aaa` /
/// `/dev/mapper/braid-bbb` in lock tests.
pub(crate) fn lock_test_membership() -> PoolMembership;

// ---------------------------------------------------------------------------
// Composite runners
// ---------------------------------------------------------------------------

/// Adds the canonical `BtrfsFilesystemShow /mnt/storage` mock (two-disk pool
/// at fsid aaaaaaaa-...) to a runner. Use when you build a runner from
/// scratch but still need to reach the "mounted, btrfs" preflight branch.
pub(crate) fn lock_with_fsid_probe_mocks(runner: MockRunner) -> MockRunner;

/// Pre-built runner for happy-path mounted lock: mountpoint=ok, fsid probe,
/// umount=ok, forget=ok for [/dev/mapper/braid-aaa, /dev/mapper/braid-bbb].
/// Per-device CryptsetupStatus / CryptsetupLuksUuid intentionally absent --
/// `MissingMock` panic is the regression guard that lock no longer issues
/// them at runtime.
pub(crate) fn lock_mounted_runner() -> MockRunner;

/// Pre-built runner for umount-busy scenarios: mountpoint=ok, fsid probe,
/// umount=err(32, "target is busy"). `BtrfsDeviceScanForget` intentionally
/// absent -- forget is gated on successful unmount.
pub(crate) fn lock_umount_failed_runner() -> MockRunner;

// ---------------------------------------------------------------------------
// Dry-run assertion helpers
// ---------------------------------------------------------------------------

/// Extract the devices list from the (exactly one expected)
/// `BtrfsDeviceScanForget` step in a compiled plan. Panics on zero or
/// multiple forget steps.
pub(crate) fn lock_forget_step_devices(steps: &[Step]) -> Vec<String>;

/// Count `BtrfsDeviceScanForget` steps in a step list. Used to assert
/// that the empty-mappers branch omits the step entirely (an empty forget
/// invocation would be kernel-global and incorrect).
pub(crate) fn lock_count_forget_steps(steps: &[Step]) -> usize;
```

### B. Re-exports under lock-prefixed aliases

The facade exposes lock's runtime test helpers under lock-prefixed names so
the C2-C7 imports do not collide with the same-named locals still present
in `lock.rs::tests`. This matches the precedent set by `enroll_key_file`
at `test_fixtures/enroll_key_file.rs:58` (`err_raw as enroll_err_raw`).

Two flavors:

**B.1: Mount helpers re-exported under lock-prefixed aliases.**

| Aliased name | Source | Notes |
|---|---|---|
| `LockNoopSleeper` | `mount::NoopSleeper` | Body is `fn sleep(&self, _: Duration) {}` -- byte-identical to lock's local. |
| `lock_ok_raw` | `mount::ok_raw` | Byte-identical signature + body. |
| `lock_err_raw` | `mount::err_raw` | Byte-identical signature + body. |
| `lock_test_config` | `mount::test_config` | Both call `Config::new(MountPoint("/mnt/storage".into())).unwrap()`. |

If mount's underlying definitions ever drift in a way lock should not
follow, the alias declaration in `test_fixtures/lock.rs` can be replaced
with a fresh in-module body without churning any test import line.

**B.2: Lock-internal type re-exported under a lock-prefixed alias.**

| Aliased name | Source | Notes |
|---|---|---|
| `LockRecordingRunner` | `lock::RecordingRunner` (this module) | The struct is declared in `test_fixtures/lock.rs` as `RecordingRunner`; the facade re-export aliases it so test imports do not collide with the same-named local `struct RecordingRunner` in `lock.rs::tests` until C8 deletes it. The aliased name stays after C8, consistent with `LockNoopSleeper`. |

**Reused as-is.** `shared::MockFs` is reused as-is via the `lock_fs()`
wrapper in Section A plus the builder methods added in Section C.

### C. Additive changes to `shared::MockFs`

`shared::MockFs` already supports `paths`, `mountinfo`, and `excl_op` with
builder `with_excl_op`. Lock needs three more knobs. All changes are
additive; no existing migrated test exercises `list_dir` or overrides
mountinfo, so behavior for current callers is unchanged.

```rust
// New private state on shared::MockFs
enum DevMapperListing {
    Empty,
    Entries(Vec<String>),
    Error(std::io::ErrorKind),
}

impl MockFs {
    /// Override `/proc/self/mountinfo` body. Use for "mounted but
    /// non-btrfs" tests in lock and any future caller that needs a custom
    /// mount line.
    pub(crate) fn with_mountinfo(mut self, body: &str) -> Self;

    /// Configure `list_dir("/dev/mapper")` to return `entries`. Use for
    /// orphan-scan tests in lock.
    pub(crate) fn with_dev_mapper(mut self, entries: &[&str]) -> Self;

    /// Configure `list_dir("/dev/mapper")` to return `PermissionDenied`.
    /// Use for orphan-scan-failure tests in lock; replaces the per-test
    /// inline `FailListDirFs` structs.
    pub(crate) fn with_dev_mapper_error(mut self) -> Self;
}
```

`list_dir` is updated to honor the new field for `/dev/mapper`; other
paths still return `Ok(vec![])`. The `storage()` and `unmounted()`
constructors initialize the new field to `DevMapperListing::Empty`.

### D. Stays inline in `lock.rs::tests` (intentional)

| Item | Rationale |
|---|---|
| `RecordingSleeper` | Records `Duration` values; used by exactly one test (`close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts`) that exercises the helper-level `close_mapper_with_retry`. The struct's only purpose is to assert sleep durations -- promoting it to the fixture would obscure that it is the test subject, not a generic harness. |
| `cmd_lock_wrapper_uses_real_sleeper` | This test must use `RealSleeper` (calls public `cmd_lock`, not `cmd_lock_impl`). It is the single regression guard that the production wrapper wires the real clock. Do not swap in `NoopSleeper` -- it would invalidate the proof. |
| `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts` | Untouched; uses inline `RecordingSleeper` (above). The helper-level test is intentionally narrower than `cmd_lock_impl` and should stay narrow. |

### E. Naming convention for lock fixtures

Following the precedent set by `unlock_*`, `enroll_*`, and `status_*`:

- Free functions on the facade: prefix with `lock_*` (e.g.
  `lock_with_fsid_probe_mocks`, `lock_mounted_runner`,
  `lock_test_membership`, `lock_forget_step_devices`, plus the aliased
  `lock_ok_raw` / `lock_err_raw` / `lock_test_config` from Section B).
- Lock-internal types are declared without a prefix inside
  `test_fixtures/lock.rs` (e.g. `pub(crate) struct RecordingRunner`) but
  the facade re-exports them under a lock-prefixed alias whenever
  `lock.rs::tests` carries a same-named local that survives into C7
  (e.g. `RecordingRunner as LockRecordingRunner`).
- Aliased mount types follow the same pattern: prefix to avoid
  collision (`LockNoopSleeper` re-exports `mount::NoopSleeper`).
- The `lock_` / `Lock` prefix is load-bearing in two ways:
  1. Avoids collision with mount's `test_config`, `NoopSleeper`,
     `ok_raw`, `err_raw`, `two_disk_membership`, and any future facade
     additions.
  2. Lets the staged migration import a fixture helper while the
     same-named local still exists in `lock.rs::tests` (we delete the
     locals only in the final cleanup commit).

## Migration ordering principle

**Hard cases first, bulk middle, cleanup last.** Each sub-commit keeps
`just test-rust` green and only migrates a behavior family it can finish
end-to-end in one diff. RecordingRunner-heavy tests are split off into a
late commit because they exercise the most complex helper -- if the
RecordingRunner shape needs adjustment, we want to discover that after
the simpler fixture surface has stabilized.

**Per-test migration rule (applies to every migrated test in C2-C7).**
Each migrated test body must replace **every** local-helper reference
with its lock-fixture counterpart in the same diff -- never leave a half-
migrated test that still calls a local name. The full swap table:

| Local | Lock fixture |
|---|---|
| `MockFs::new(...)` (and `.with_exclop(...)` / `.with_mountinfo(...)`) | `lock_fs(...)` (chained `.with_excl_op(...)` / `.with_mountinfo(...)` from `shared::MockFs`) |
| `mounted_runner()` | `lock_mounted_runner()` |
| `umount_failed_runner()` | `lock_umount_failed_runner()` |
| `with_fsid_probe_mocks(...)` | `lock_with_fsid_probe_mocks(...)` |
| `test_config()` | `lock_test_config()` |
| `test_membership()` | `lock_test_membership()` |
| `NoopSleeper` | `LockNoopSleeper` |
| `ok_raw(...)` | `lock_ok_raw(...)` |
| `err_raw(...)` | `lock_err_raw(...)` |
| `forget_step_devices(...)` | `lock_forget_step_devices(...)` |
| `count_forget_steps(...)` | `lock_count_forget_steps(...)` |
| `RecordingRunner` | `LockRecordingRunner` (facade alias for `lock::RecordingRunner`; the local type also named `RecordingRunner` would otherwise shadow the import through C7) |

A test that still references a local-only name will compile through C7
(the locals are intact until C8) but breaks at C8 cleanup. The per-test
verification gate in each commit (`cargo test --lib lock::tests::<name>`)
catches behavior regressions, but not stale-name regressions; the
**discipline is on the patch author** to grep the migrated test body for
any of the un-prefixed names above before committing. C8's
`cargo check --tests` will fail loudly if any leak through.

## Sub-commit plan

| # | Commit subject | Scope | Validates |
|---|---|---|---|
| C1 | `refactor(test): add lock-scope test fixture module` | Add `cli/src/test_fixtures/lock.rs` declaring `RecordingRunner`, `lock_fs`, `lock_with_fsid_probe_mocks`, `lock_test_membership`, `lock_mounted_runner`, `lock_umount_failed_runner`, `lock_forget_step_devices`, `lock_count_forget_steps`, plus the alias re-exports (`mount::NoopSleeper as LockNoopSleeper`, `mount::ok_raw as lock_ok_raw`, `mount::err_raw as lock_err_raw`, `mount::test_config as lock_test_config`). Extend `shared::MockFs` with `with_mountinfo`, `with_dev_mapper`, `with_dev_mapper_error` (and the private `DevMapperListing` field). Add `mod lock;` and `#[allow(unused_imports)] pub(crate) use lock::{... lock::RecordingRunner as LockRecordingRunner, LockNoopSleeper, lock_err_raw, lock_ok_raw, lock_test_config, ...};` to `cli/src/test_fixtures.rs`. Apply `#[allow(dead_code)]` on the new module's items. **No changes to `cli/src/lock.rs`.** | `cargo check --manifest-path cli/Cargo.toml --tests`; `just test-rust` (must stay green; no test logic change). |
| C2 | `refactor(lock): migrate happy-path and forget tests to shared fixtures` | Migrate Group A (6 tests): `lock_happy_path_unmounts_and_closes`, `execute_does_not_close_membership_mapper_absent_from_plan`, `lock_already_locked`, `lock_partial_state`, `lock_adds_forget_after_umount`, `lock_forget_failure_is_nonfatal`. Per-test call-site swaps: `MockFs::new(...)` -> `lock_fs(...)`; `mounted_runner()` -> `lock_mounted_runner()`; `umount_failed_runner()` -> `lock_umount_failed_runner()`; `test_config()` -> `lock_test_config()`; `test_membership()` -> `lock_test_membership()`; `NoopSleeper` -> `LockNoopSleeper`; `ok_raw` -> `lock_ok_raw`; `err_raw` -> `lock_err_raw`. The lock-prefixed aliases avoid the import collision with the still-present locals -- **do not delete the local helpers in this commit**, they are still used by Groups B-G. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_happy_path_unmounts_and_closes` (etc., per test); `just test-rust`. |
| C3 | `refactor(lock): migrate umount-failure variant tests to shared fixtures` | Migrate Group B (4 tests): `lock_umount_busy_fails`, `lock_umount_busy_includes_hint`, `lock_umount_non_busy_omits_hint`, `lock_umount_path_containing_busy_phrase_omits_hint`. Apply the per-test migration rule end-to-end. These layer custom umount stderr onto `lock_umount_failed_runner()` via `.with_output(Umount, lock_err_raw(...))` overrides. Preserve the exact stderr strings -- `"target is busy"`, `"can't write superblock"`, and the path-with-busy-phrase variant -- because hint gating depends on diagnostic-segment text. Preserve `assert!(msg.contains("lsof") && msg.contains("fuser"))` vs. `!contains("lsof") && !contains("fuser")` precisely. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_umount_busy_fails` (etc.); `just test-rust`. |
| C4 | `refactor(lock): migrate orphan and dry-run preview tests to shared fixtures` | Migrate Group C (6 tests): `lock_closes_orphaned_mapper`, `lock_orphan_scan_failure_is_nonfatal`, `dry_run_preview_warns_when_list_dir_fails`, `dry_run_preview_warns_per_orphan_mapper`, `dry_run_preview_mounted_happy_path`, `dry_run_preview_nothing_to_do`. Orphan-present variants use `lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb", "/dev/mapper/braid-ccc"])` -- the `/dev/mapper` listing is auto-derived from `paths` (Section A), so no `.with_dev_mapper(...)` chain is needed. Both `FailListDirFs` call sites become `lock_fs(...).with_dev_mapper_error()`; delete the two inline `FailListDirFs` structs. Preserve every dry-run-output `assert!(output.starts_with(...))` and `assert!(output.contains(...))` byte-for-byte. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_closes_orphaned_mapper` (etc.); `just test-rust`. |
| C5 | `refactor(lock): migrate error-precedence tests to shared fixtures` | Migrate Group D (7 tests): `lock_umount_fails_but_mappers_close_successfully`, `lock_umount_fails_busy_mapper_is_warning`, `lock_umount_fails_unexpected_mapper_error_is_fatal`, `lock_mapper_close_fatal_when_umount_succeeded`, `lock_umount_fails_orphan_busy_is_warning`, `lock_umount_fails_orphan_unexpected_error_is_fatal`, `lock_orphan_close_failure_is_fatal`. Each test layers custom mapper-close outputs onto `lock_umount_failed_runner()` or `lock_mounted_runner()`. Preserve which specific error variant must be returned (umount-error vs. mapper-error) -- these assertions encode the precedence contract and must round-trip exactly. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_umount_fails_but_mappers_close_successfully` (etc.); `just test-rust`. |
| C6 | `refactor(lock): migrate RecordingRunner tests to shared fixtures` | Migrate Group E (5 tests): `lock_continues_closing_after_mapper_error`, `lock_collects_first_mapper_error`, `lock_retries_busy_close_then_succeeds`, `lock_mapper_close_exit5_is_busy_regardless_of_wording`, `lock_busy_close_exhausts_retries_preserves_stderr_contract`. Apply the per-test migration rule end-to-end. Use the facade `LockRecordingRunner` (aliased from `lock::RecordingRunner` in Section B.2) -- not the bare `RecordingRunner` name, which would silently resolve to the still-present local struct in `lock.rs::tests` and bypass the migration. Verify the recorded `close_calls()` lengths and ordering exactly. For `lock_busy_close_exhausts_retries_preserves_stderr_contract`, preserve the exact `err.to_string()` equality (not `contains`); the trimmed-stderr text is part of the contract. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_continues_closing_after_mapper_error` (etc.); `just test-rust`. |
| C7 | `refactor(lock): migrate refusal, dry-run render, and forget-recording tests` | Migrate Groups F + G (11 tests): refusal/dry-run-render -- `lock_refuses_when_exclusive_op_active`, `lock_refuses_when_balance_paused`, `lock_rejects_mounted_but_not_btrfs`, `dry_run_render_lock_mounted_2_disks`, `dry_run_lock_not_mounted_1_open`, `dry_run_lock_nothing_to_do`, `dry_run_lock_forget_step_lists_scoped_devices`, `dry_run_lock_forget_step_includes_orphans`, `dry_run_lock_forget_step_omitted_when_no_mappers`. Plus forget-recording: `lock_forget_is_pool_scoped`, `lock_forget_includes_orphan_mappers`. Apply the per-test migration rule end-to-end. Use `lock_fs(...).with_excl_op("balance")` and `.with_mountinfo("...ext4...")` for the refusal tests; `lock_forget_step_devices` / `lock_count_forget_steps` for the dry-run renders; `LockRecordingRunner` for the forget-recording tests (same aliasing rationale as C6). Preserve exact step-count asserts (e.g. line counts in `dry_run_render_lock_mounted_2_disks`). | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_refuses_when_exclusive_op_active` (etc.); `just test-rust`. |
| C8 | `chore(lock): drop migrated locals from lock.rs tests module` | Delete the local `RecordingRunner`, `MockFs`, `NoopSleeper`, `ok_raw`, `err_raw`, `with_fsid_probe_mocks`, `test_config`, `test_membership`, `mounted_runner`, `umount_failed_runner`, `forget_step_devices`, `count_forget_steps` from `cli/src/lock.rs::tests`. Drop now-unused `use` statements. Keep inline: `RecordingSleeper` (used by `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts`) and `cmd_lock_wrapper_uses_real_sleeper`. **Keep** the lock-prefixed alias re-exports (`LockNoopSleeper`, `LockRecordingRunner`, `lock_ok_raw`, `lock_err_raw`, `lock_test_config`) on the facade -- they are the post-migration test-facing names; renaming them again would churn every C2-C7 import. Remove `#[allow(unused_imports)]` from the lock facade re-export and `#[allow(dead_code)]` from the `lock` module if everything is consumed. | `cargo test --manifest-path cli/Cargo.toml --lib lock::tests`; `just test-rust`; `cargo check --manifest-path cli/Cargo.toml --tests` (must show no dead-code warnings). |

## Critical files to modify

- `cli/src/test_fixtures.rs` -- add `mod lock;` and the re-export block (C1, C8 cleanup)
- `cli/src/test_fixtures/shared.rs` -- add `with_mountinfo`, `with_dev_mapper`, `with_dev_mapper_error`, the private `DevMapperListing` enum, and update `list_dir` (C1)
- `cli/src/test_fixtures/lock.rs` -- new file with all lock-scoped helpers (C1)
- `cli/src/lock.rs` -- migrate tests in lock.rs:559–2556 across C2–C7; delete locals in C8

## Existing functions / utilities reused

- `shared::MockFs`, `shared::MockFs::storage`, `shared::MockFs::with_excl_op` (existing)
- `mount::NoopSleeper`, `mount::ok_raw`, `mount::err_raw`, `mount::test_config` (re-exported under lock-prefixed aliases per Section B)
- `MockRunner::default`, `MockRunner::with_output` (existing)
- `cli/src/lock.rs::compile_lock_steps`, `LockPlan::execute`, `cmd_lock_impl`, `plan_lock` (existing; no changes to production code)
- The `enroll_err_raw` aliasing precedent in `cli/src/test_fixtures/enroll_key_file.rs:53-58` (pattern only; no code reuse)

## Out of scope for this plan

- Promoting `NoopSleeper` to `shared` -- mount, recover, and lock all carry
  their own; the duplication is small and existing precedent leaves it
  per-module.
- Generalizing `with_dev_mapper` / `with_dev_mapper_error` to arbitrary
  directory listings (e.g. a `with_dir_listing(path, entries)` builder).
  Lock only scans `/dev/mapper`; a future migration can broaden if needed.
- Refactoring lock production code (`cmd_lock_impl`, `LockPlan::execute`,
  `compile_lock_steps`, etc.). Tests-only migration.
- The two intentionally-untouched real-clock tests (`close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts`, `cmd_lock_wrapper_uses_real_sleeper`) and their inline helpers.
- Replacing `MockRunner::with_output` chains with `with_handler` closures.
  Lock tests do not currently use `with_handler`, and several rely on the
  `MissingMock` panic from `with_output` to prove a command was not issued.

## Risks

| ID | Risk | Mitigation |
|---|---|---|
| R1 | Extending `shared::MockFs::list_dir` changes its contract for unmigrated callers. | The change is gated on `path == "/dev/mapper"`, and the default `DevMapperListing::Empty` returns `Ok(vec![])` (the current behavior). No existing migrated test calls `list_dir`. C1 verification (`just test-rust`) confirms zero behavior change for current callers. |
| R2 | Subtle drift between mount's helpers (`test_config`, `NoopSleeper`, `ok_raw`, `err_raw`) and lock's expectations (e.g. mount adds a new field to `test_config` later). | The lock-prefixed alias names (`lock_test_config`, `LockNoopSleeper`, `lock_ok_raw`, `lock_err_raw`) decouple lock's import path from mount's underlying definitions. If drift surfaces, the alias declaration in `test_fixtures/lock.rs` can be replaced with a fresh in-module body without touching any test import line. Section B documents the rationale and the `enroll_err_raw` precedent. |
| R8 | Auto-deriving `/dev/mapper` listing from `paths` in `lock_fs` causes a regression if a future caller depends on the previous "always-empty list_dir" semantics. | `shared::MockFs::list_dir` itself remains "empty unless overridden" -- only `lock_fs` does the derivation, and only because it is what the local mock at `lock.rs:683-696` already does. Other callers continue to call `shared::MockFs::storage(...)` directly and observe unchanged behavior. |
| R3 | `RecordingRunner` extraction subtly changes lock-relevant behavior (e.g. response-queue draining order). | C1 lifts the struct verbatim from `lock.rs:576–634`; no body changes. C6 is the consumer commit and validates per-test (close_calls length, ordering, retry counts). |
| R4 | Migrating dry-run preview tests breaks exact output snippets (e.g. `output.starts_with("[warn] could not scan...")`). | Each dry-run test in C4/C7 must keep the same assertion strings. Migrate one test at a time within the commit and run that test in isolation before moving on. The `lock_fs(...)` helper does not change rendered output -- the only call-site change is the filesystem construction. |
| R5 | Conflating `lock_mounted_runner()` and `lock_umount_failed_runner()` in the wrong tests (e.g. seeding forget=ok where the test relies on its absence to prove forget was NOT issued after umount failure). | The new `lock_umount_failed_runner` deliberately omits `BtrfsDeviceScanForget`, mirroring the local. Per-test review in C3/C5 confirms the right runner is chosen. The `MissingMock` panic is the safety net. |
| R6 | Removing `FailListDirFs` in C4 silently changes `read_to_string` semantics for the dry-run-preview-on-not-mounted test (where the original returned `NotFound` for any path). | The not-mounted dry-run test never reads `/proc/self/mountinfo` (mountpoint check returns err 1), so the body of `read_to_string` is unobserved. Use `lock_fs(...).with_dev_mapper_error()` for both `FailListDirFs` replacements; it preserves the observed `exists` and `list_dir` behavior while keeping call sites lock-prefixed. Validate by running both tests in isolation in C4. |
| R7 | `lock_test_membership` accidentally diverges from the `aaa`/`bbb` short-name convention used across all 41 tests, breaking forget-set asserts and orphan-mapper detection. | C1 lifts the membership verbatim from `lock.rs:750–761`. C7 (forget-recording) is the explicit validation: `lock_forget_is_pool_scoped` checks `[["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]]` exactly. |

## Verification

### Per sub-commit

```bash
# After C1 (scaffold + shared extension):
cargo check --manifest-path cli/Cargo.toml --tests
just test-rust

# After C2-C7 (migration commits):
# Run the specific tests being migrated in this commit, e.g. for C3:
cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_umount_busy_fails
cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_umount_non_busy_omits_hint
cargo test --manifest-path cli/Cargo.toml --lib lock::tests::lock_umount_path_containing_busy_phrase_omits_hint

# Then the full lock module:
cargo test --manifest-path cli/Cargo.toml --lib lock::tests

# Then the gate:
just test-rust

# After C8 (cleanup):
cargo check --manifest-path cli/Cargo.toml --tests   # no dead-code warnings
cargo test --manifest-path cli/Cargo.toml --lib lock::tests
just test-rust
```

### End-to-end

After C8 lands:

- `just test-rust` -- full Rust unit-test suite green.
- `git diff master -- cli/src/lock.rs | wc -l` should show a substantial
  net deletion (target: ~150-200 lines removed from `lock.rs`'s test
  module, balanced by ~150-200 lines added under `cli/src/test_fixtures/`).
- Running each behavior-family test individually after C8 should still
  produce the same pass/fail signal as before C1 -- no test changes
  semantics, only call-site wiring.

## Branch and commit shape

- Conventional Commits style; lowercase first word per AGENTS.md.
- Subjects:
  - C1: `refactor(test): add lock-scope test fixture module`
  - C2-C7: `refactor(lock): migrate <family> tests to shared fixtures`
  - C8: `chore(lock): drop migrated locals from lock.rs tests module`
- Each commit must pass `just test-rust` independently.
- No unrelated changes piggy-backed on any commit.
