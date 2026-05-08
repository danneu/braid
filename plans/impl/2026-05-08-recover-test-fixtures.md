# Plan: Migrate `recover.rs` Tests to Shared Test Fixtures

## Context

This is the fifth and largest scope migration in the test-fixtures
program initiated by `plans/impl/2026-05-07-mockrunner-handler-and-shared-test-fixtures.md`.
The prior four (`replace`, `add`, `remove`, `remove_missing`) consolidated
per-test scaffolding (`*Runner` structs, inline `tempdir + config + pass +
membership` setups) into a `PoolFixture` + per-scope topology installer +
per-scope params-builder model that uses `MockRunner::with_handler` for
broad topology mocks and reverse-order dispatch for per-test overrides.

`recover.rs` is the largest file in the CLI (14471 lines, ~3267 of
production code, ~11200 of tests + scaffolding) and the most behaviorally
diverse. Its 116 tests span add-replay, remove-missing-replay,
replace-replay, add/remove/replace post-maintenance, kernel
dev_replace polling, plan/pre-mount discovery, remount cycle (stateful
mount/unmount), inhibitor-failure boundaries, by-id resolution,
guidance helpers, dry-run preview rendering, and a long tail of error
paths. The module is also the only command that integrates *all*
existing scope topologies (it replays committed mutations from any of
add/remove/remove-missing/replace journals), which makes any "broad
recover topology" mock rigid by construction.

The intended outcome is the same as prior migrations: shrink per-test
scaffolding to fixture calls + per-test overrides, drop dead helpers,
and put the remaining sharp local fakes (request-order observers,
journal/pool.json preservation guards, mount/remount state mutators,
by-id resolvers, inhibitor stubs, and replace-status sequence runners)
exactly where the user-visible test reads them. No production code
changes.

## Inventory by Behavior Family

The test module has 116 `#[test]` functions. Grouped by behavior, with
disposition (Migrate / Stay-local / Reuse-existing-fixture):

| Family                                        | Count | Disposition       | Notes                                                                                       |
| --------------------------------------------- | ----- | ----------------- | ------------------------------------------------------------------------------------------- |
| Kernel dev_replace polling                    | 11    | Stay-local        | `ReplaceStatusSequenceRunner` is a sharp local fake (per user guidance). No `RecoverParams`. |
| By-id resolution                              | 2     | Stay-local        | `MockByIdResolver` directly tests `resolve_by_id_for_underlying`. No `RecoverParams`.       |
| Guidance helpers                              | 9     | Stay-local        | Pure data-driven; no runner, no fixture.                                                    |
| Add post-maintenance / replay (unit-level)    | 2     | Migrate           | `execute_add_post_balance_recovery`, `execute_add_pool_mutation_recovery` direct callers.   |
| Add pool-mutation replay (cmd-level)          | 7     | Migrate           | Full `cmd_recover` flow with add-flavored journal.                                          |
| Render add recovery (preview)                 | 2     | Migrate           | `render_add_recovery_*`; build a plan and snapshot rendered steps.                          |
| Remove-missing pool-mutation replay           | 5     | Migrate           | Full `cmd_recover` with remove-missing journal.                                             |
| Remove-missing post-maintenance               | 3     | Migrate           |                                                                                             |
| `cmd_recover_remove_with_*`                   | 3     | Migrate           | Recovery of an interrupted `Remove`.                                                        |
| Replace pool-mutation replay (committed/etc.) | 3     | Migrate           |                                                                                             |
| Replace fresh-LUKS replay variants            | 5     | Migrate           | Header-backup, wrong label, absent target, bad passphrase.                                  |
| Replace existing-LUKS-with-enroll variants    | 3     | Migrate           |                                                                                             |
| Replace post-maintenance                      | 3     | Migrate           |                                                                                             |
| Plan / pre-mount discovery                    | 5     | Migrate           |                                                                                             |
| Remount cycle (stateful mount/unmount)        | 9     | Migrate           | Uses `StatefulMockFs` + `MapperClosingRunner`; promote to fixture as a remount harness.     |
| Inhibitor failure                             | 5     | Migrate           | `FailingInhibitor` and `RequestCountInhibitor` stay local; tests adopt fixture for the rest.|
| Dry-run                                       | 2     | Migrate           |                                                                                             |
| Preview / dry-run rendering                   | 18    | Migrate           | Largely input-driven; minimal runner needs.                                                 |
| `recover_*` end-to-end (mount, mid-add, etc.) | 14    | Migrate           | Bootstrap, paused balance, by-id staleness, added_at carrying.                              |
| Cross-family helpers (state)                  | (n/a) | Stay-local        | `pool_state_*`, journal builders -- recover-specific data shapes.                           |

Total: ~22 stay-local + ~94 migrate. Stay-local tests still benefit
from facade re-exports if anything they need moves to a fixture, but
their bodies don't change.

## Inventory of Current Scaffolding

Test-mod scaffolding in `cli/src/recover.rs:3268..3631` and below.
Disposition classified the same way:

**Stay-local (sharp fakes per user guidance):**

- `ReplaceStatusSequenceRunner` (3580-3614) -- polling sequence, asserts only
  `BtrfsReplaceStatus` requests reach the runner. Used by 11 tests.
- `MockByIdResolver` + `resolver_for(...)` (3509-3559) -- by-id resolver
  test fake. Used by ~7 tests (2 direct + 5 transitively).
- `RequestCountInhibitor` (3348-3381) -- counts inhibitor acquires + records
  request-count at first acquire (for seam-placement assertions). Used by 4 tests.
- `FailingInhibitor` (3340-3346) -- always errors. Used by 5 inhibitor-failure tests.
- `NoopInhibitor` + `NOOP_INHIBITOR` static (3330-3338) -- the default inhibitor.
- `passphrase`, `write_valid_keyfile`, `set_of`, `ref_set` -- tiny test helpers.

**Promote to `cli/src/test_fixtures/recover.rs`:**

- `MockFs` (3289-3323) -- a recover-local clone of the shared `MockFs`. The
  shared `MockFs::storage()` and `MockFs::unmounted()` already cover the
  same surface; tests will adopt the shared one.
- `StatefulMockFs` + `MapperClosingRunner` + `SharedPaths` (3387-3502) -- the
  remount-cycle harness. Promote together as `RemountHarness` (or a struct
  exposing `(runner_wrapper, fs, paths_handle)`).
- `recover_params` / `recover_params_with_inhibitor` (4951-4977) -- the obvious
  builder seed.

**Promote *as journal/state builders that live next to recover fixtures*
or stay local at the module's preference (low cost either way):**

- 14 journal builders (`two_disk_journal`, `committed_two_disk_add_journal`,
  `recoverable_pool_mutation_add_journal`, `remove_missing_journal`,
  `replace_journal`, `replace_fresh_luks_journal`,
  `replace_existing_luks_with_enroll_journal`, `bootstrap_journal`,
  `interrupted_remove_journal`, `remove_2to1_journal`, etc.) -- recover-specific.
  **Recommend: keep local.** They are not reused outside recover; promoting
  adds churn without eliminating it.
- 7 pool-state builders (`pool_state_one_disk`, `pool_state_two_disks`,
  `pool_state_three_disks`, `pool_state_disk1_and_old`, etc.) --
  recover-specific. **Recommend: keep local.**
- Command-output helpers (`ok_raw`, `ok_raw_empty`, `err_raw`,
  `cryptsetup_status_active`, `cryptsetup_uuid_ok`, `btrfs_show_*`,
  `luks_dump_label`) -- recover-specific shapes. **Recommend: keep local.**
- Runner builder chains (`already_mounted_one_disk_runner`,
  `with_one_disk_pool_probe`, `with_two_disk_pool_probe`,
  `with_three_disk_pool_probe`, `with_balance_replay`) -- these are
  pre-`with_handler` chained `.with_output` builders. **Recommend: keep
  local at first**; revisit at cleanup whether two of them collapse to
  a `with_handler` closure.

**Reuse existing shared/scope fixtures:**

- `PoolFixture::two_disk_healthy()` (`shared.rs:129`) -- live disk1 + disk2.
- `PoolFixture::live_one_disk()` (`add.rs:28`) -- live disk1 only.
- `PoolFixture::three_disk_devids_pinned()` (`remove_missing.rs:165`) --
  three-disk pool with pinned devids.
- `PoolFixture::two_disk_devids_pinned()` (`remove_missing.rs:189`) --
  two-disk pool with pinned devids.
- `PoolFixture::empty()` (`shared.rs:178`) -- no pool.json seeded.
- `MockFs::storage(paths)` (`shared.rs:41`), `MockFs::unmounted(paths)` (`shared.rs:54`).
- `mock_ok` (`shared.rs:16`).
- `MockRunner::with_handler` (`cmd.rs:1021`).
- `MockRunner::with_luks_dump_text_luks2_for` (`cmd.rs:1101`).
- `MockRunner::with_mappers_closed` (`cmd.rs:1126`).

## Fixture Design

### New module: `cli/src/test_fixtures/recover.rs`

Three additions, no broad topology installer.

**1. `RecoverParamsBuilder<'a>` over `RecoverParams<'a>`.**

```rust
pub(crate) struct RecoverParamsBuilder<'a> {
    config: &'a Config,
    paths: &'a StatePaths,
    passphrase_stdin: bool,
    passphrase_file: Option<&'a Path>,
    allow_degraded: bool,
    dry_run: bool,
    progress: ProgressOutput,
    sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}
```

Setters (only what tests vary):
`.dry_run()`, `.allow_degraded()`, `.passphrase_file()`,
`.passphrase_stdin()`, `.sleep_inhibitor()`, `.progress()`.

Defaults match the most common test shape: `dry_run=false`,
`allow_degraded=false`, `progress=Off`, `passphrase_stdin=false`,
`passphrase_file=Some(fixture pass_path)`, `sleep_inhibitor=&NOOP`.
Tests pass a custom `&dyn AcquireSleepInhibitor` via `.sleep_inhibitor(...)`
to thread `FailingInhibitor` or `RequestCountInhibitor`; no need to
extend the shared `inhibitor` field on `PoolFixture`.

**Passphrase-bytes migration rule** (load-bearing for stdin
expectations). The shared `PoolFixture` writes `b"test-passphrase\n"`
to its `pass_path` (`shared.rs:122-123`); `read_passphrase` strips the
trailing `\n`, so the bytes that reach `cryptsetup` stdin are
`b"test-passphrase"`. Pre-migration recover tests instead create their
own `NamedTempFile`, write `b"testpass"`, and set
`with_output_stdin(..., b"testpass".to_vec(), ...)` to match.

Make the byte sequence a real single source of truth so the file
contents and stdin expectations cannot drift. PR 1 commit 1 adds the
constant in `cli/src/test_fixtures/shared.rs` (NOT in recover-scope or
recover.rs), and `empty_inner` consumes it when writing `pass_path`:

```rust
// cli/src/test_fixtures/shared.rs
pub(crate) const TEST_PASSPHRASE_BYTES: &[u8] = b"test-passphrase";

pub(in crate::test_fixtures) fn empty_inner() -> ... {
    ...
    let mut pass_bytes = TEST_PASSPHRASE_BYTES.to_vec();
    pass_bytes.push(b'\n');                           // trailing newline
    std::fs::write(&pass_path, &pass_bytes).expect("write passphrase file");
    ...
}
```

Then re-export from the facade so recover (and any future scope) can
adopt it:

```rust
// cli/src/test_fixtures.rs
pub(crate) use shared::{MockFs, PoolFixture, TEST_PASSPHRASE_BYTES, mock_ok};
```

After migration, tests have two paths and must pick one explicitly:

1. **Adopt the fixture default.** Drop the local `NamedTempFile` and
   change every stdin expectation from `b"testpass".to_vec()` to
   `TEST_PASSPHRASE_BYTES.to_vec()`. The shared constant lives next to
   the file-write that produces it, so a future change to the bytes is
   one diff that propagates to every adopting test automatically.
2. **Override.** Tests that exercise `passphrase_file=None`, a
   wrong-passphrase path (`replace_pool_mutation_fresh_luks_bad_passphrase_preserves_journal`,
   `recover_bootstrap_wrong_passphrase_not_masked`, etc.), or any
   bespoke passphrase scenario keep their local `NamedTempFile` and
   call `.passphrase_file(Some(local.path()))` (or `.passphrase_file(None)`)
   on the builder. These tests retain whatever stdin bytes they were
   already using.

The bulk migration commits (PR 2 commits 5-12) must apply rule 1 by
default and rule 2 only where the test explicitly tests the
non-default credential path. Audit each commit by running
`rg -n 'b"testpass"' cli/src/recover.rs` before staging; the count
should monotonically decrease as families migrate, and a final
post-cleanup run should return only the rule-2 overrides (or zero, if
the override path also adopts a different shared constant).

**2. `Config` field on `PoolFixture` (extends `shared.rs`).**

`RecoverParams` borrows `&'a Config`, not `&'a Path`. The simplest fix is
to store an owned `Config` on `PoolFixture` so `recover_params(&self)`
can borrow it. The field is added to `shared.rs::PoolFixture` and
populated in `empty_inner()` from the same `mount_point` already used
to write `config.json`. `Config` is `Clone`, so the change is drop-in.

```rust
pub(crate) struct PoolFixture {
    pub(in crate::test_fixtures) _state_tmp: TempDir,
    pub(crate) paths: StatePaths,
    pub(in crate::test_fixtures) _config_tmp: TempDir,
    pub(crate) config_path: PathBuf,
    pub(crate) config: Config,          // NEW
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
}
```

Then in `recover.rs`:

```rust
struct NoopInhibitor;
impl AcquireSleepInhibitor for NoopInhibitor {
    fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn SleepGuard>> {
        Ok(Box::new(()))
    }
}

static RECOVER_NOOP_INHIBITOR: NoopInhibitor = NoopInhibitor;

impl PoolFixture {
    pub(crate) fn recover_params(&self) -> RecoverParamsBuilder<'_> {
        RecoverParamsBuilder {
            config: &self.config,
            paths: &self.paths,
            passphrase_stdin: false,
            passphrase_file: Some(self.pass_path.as_path()),
            allow_degraded: false,
            dry_run: false,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &RECOVER_NOOP_INHIBITOR,  // 'static, coerces to 'a
        }
    }
}
```

The fixture module declares its own `NoopInhibitor` + a `'static`
binding (`RECOVER_NOOP_INHIBITOR`) so the default-arm builder doesn't
depend on recover.rs's `NOOP_INHIBITOR`. The `'static` reference
coerces to `'a` for any builder lifetime. recover.rs's existing
`static NOOP_INHIBITOR: NoopInhibitor = NoopInhibitor;` (at line 3338)
stays local for tests that pass it explicitly via `.sleep_inhibitor(&NOOP_INHIBITOR)`,
or those tests can drop the explicit pass since the builder default
already supplies an equivalent.

**3. `RemountHarness` (promoted `StatefulMockFs` + `MapperClosingRunner`).**

```rust
pub(crate) struct RemountHarness {
    pub(crate) fs: RemountFs,           // was StatefulMockFs
    pub(crate) runner: RemountRunner,   // was MapperClosingRunner
}

impl RemountHarness {
    /// Wrap a fully-built `MockRunner` (with all `with_output` /
    /// `with_handler` calls already applied). Tests configure the
    /// inner `MockRunner` first, then hand it to the harness.
    pub(crate) fn new(initial_paths: &[&str], inner: MockRunner,
                      already_closed: &[&str]) -> Self;

    /// Delegates to the wrapped `MockRunner::requests()`. Required
    /// because the existing test pattern is
    /// `let request_log = inner.clone(); let runner = MapperClosingRunner { inner, ... };`
    /// followed by `request_log.requests()` for ordering /
    /// "must not run" assertions. After migration tests must keep
    /// observing the same shared `MockRunner` log; this accessor is
    /// the in-fixture equivalent.
    pub(crate) fn requests(&self) -> Vec<CmdRequest>;

    /// Snapshot of the harness-owned closed-mappers set. Note that
    /// the wrapped runner short-circuits `CryptsetupStatus` for any
    /// mapper in this set *before* delegating to the inner
    /// `MockRunner`, so those short-circuited status requests are
    /// intentionally NOT in `requests()` -- this matches the
    /// pre-migration `MapperClosingRunner::run` behavior and the
    /// assertions that depend on it.
    pub(crate) fn closed_mappers(&self) -> Vec<String>;
}
```

The harness owns the `Arc<Mutex<HashSet<String>>>` for paths and the
closed-mappers set, exposes `&self.fs` and `&self.runner` for passing
to `cmd_recover`, and provides observation hooks (`requests`,
`closed_mappers`) for the post-condition asserts that the existing
tests already make.

**Recording-vs-dispatch invariant** (load-bearing for migrated tests):
`MockRunner::run` and `MockRunner::run_with_stdin`
(`cli/src/cmd.rs:1170-1199`) push the request to `self.requests`
*before* calling `self.dispatch(request)`. Handler dispatch (including
`with_handler` closures, output sequences, and static `with_output`
keys) happens after recording. This means:

- Per-test handlers registered via `with_handler` cannot suppress
  request recording -- every call into `MockRunner` shows up in
  `runner.requests()` regardless of what the handler returns.
- `RemountRunner::run` short-circuits `CryptsetupStatus` for a closed
  mapper *before* calling `self.inner.run`, so those short-circuited
  calls are NOT recorded in the inner log. This is the existing
  pre-migration behavior and tests depend on it (e.g. counting only
  the `CryptsetupStatus` calls that actually probed the kernel).
- `RequestCountInhibitor::first_acquire_request_count()` reads
  `self.runner.requests().len()` at acquire time; the count reflects
  every request issued through the inner `MockRunner` up to that point,
  including those that fell through every handler.

**No broad `RecoveryPool` topology installer.**

Recover replays committed mutations from any of add/remove/remove-missing/replace
journals, plus runs its own probe surface, plus exercises the remount cycle.
A single broad topology mock would either be too narrow (failing for cross-family
scenarios) or too broad (fixed responses that can't model the journal-driven
state transitions recover triggers). Tests will compose:

1. `MockRunner::with_handler` for the per-test broad surface (mountpoint check,
   pool probe, balance-status), one closure per test.
2. `MockRunner::with_output` / `with_output_stdin` for sharp per-call
   stubs (passphrase tests, scan-forget, mount/umount).
3. The shared `with_luks_dump_text_luks2_for` and `with_mappers_closed`
   helpers from `cmd.rs` that already exist.

### Facade re-exports (`cli/src/test_fixtures.rs`)

```rust
mod recover;

pub(crate) use recover::{RecoverParamsBuilder, RemountHarness};
```

### Test-mod side after migration (illustrative shape)

```rust
let f = PoolFixture::two_disk_healthy();
let journal = recoverable_pool_mutation_add_journal();   // local builder
journal::write_journal(&f.paths, &journal).unwrap();

let runner = MockRunner::default()
    .with_handler(|req| match req {
        // broad pool probe -- per-test
        CmdRequest::Mountpoint { .. } => Some(Ok(mock_ok("mountpoint", "/mnt/storage is a mountpoint\n"))),
        CmdRequest::CryptsetupLuksUuid { device } => luks_uuid_for(device),
        _ => None,
    });
let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);

let result = cmd_recover(
    &runner,
    &MockFs::storage(vec!["/dev/disk/by-id/virtio-disk1".into(), ...]),
    &resolver,
    &f.recover_params().build(),
);

assert!(...);
assert!(f.paths.pending_op_json().exists(), "journal preserved");
```

Tests that observe the inhibitor seam pass it explicitly:

```rust
let inhibitor = RequestCountInhibitor::new(runner.clone());
let params = f.recover_params().sleep_inhibitor(&inhibitor).build();
...
assert!(inhibitor.first_acquire_request_count().is_some());
```

## Migration Sequence

Recommended split into **two PRs**: a small design-validation PR and a
larger bulk-migration + cleanup PR. Each commit below is independently
green; if the team prefers, all 14 commits can ship as a single PR.

### PR 1: design validation (4 commits, ~3 test migrations)

| #   | Commit subject                                                                             | Notes                                                                                                                                                               |
| --- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | feat(test-fixtures): add Config field to PoolFixture and seed recover scope module         | Adds `config: Config` field + `Config::Clone` propagation; new `cli/src/test_fixtures/recover.rs` with `RecoverParamsBuilder` + `RemountHarness`; facade re-exports. |
| 2   | refactor(recover): migrate plan_recover_discovers_add_targets pre-mount test               | Hard case 1: `plan_recover_discovers_add_targets_before_mount_planning` -- uses both `StatefulMockFs` and `MapperClosingRunner`. Validates the promoted `RemountHarness` against an existing test that exercises the close-then-reopen mutation cycle end-to-end. |
| 3   | refactor(recover): migrate inhibitor-failure pool-mutation test                            | Hard case 2: `pool_mutation_inhibitor_failure_stops_before_destructive_replay` -- uses `FailingInhibitor` threaded through `.sleep_inhibitor(...)`; asserts journal preserved + zero destructive requests. Validates the params builder's custom-inhibitor path AND journal preservation. |
| 4   | refactor(recover): migrate add-mutation seam-placement test (RequestCountInhibitor)        | Hard case 3: `add_pool_mutation_replays_keyfile_enrollment_before_pool_add` -- uses `RequestCountInhibitor` to assert the inhibitor was acquired *before* destructive replay. Validates request-order observation survives the fixture (no re-ordering of recorded requests). |

**Why these three hard cases:** they exercise the three highest-risk
fixture-design properties: (1) the promoted `RemountHarness` (stateful
FS + path-mutating runner pair) against a real consumer; (2) custom
inhibitor threading via `.sleep_inhibitor(...)` plus journal/pool.json
preservation under mid-flow failure; (3) request-order observation via
a sharp local inhibitor fake. If these three pass cleanly, the
remaining ~91 migrations are mechanical applications of the same three
patterns.

### PR 2: bulk migration + cleanup (10 commits)

| #   | Commit subject                                                          | Notes                                                                                                            |
| --- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| 5   | refactor(recover): migrate replace-mutation family                      | 12 remaining tests in the replace family. RemountHarness used where stateful FS is needed.                       |
| 6   | refactor(recover): migrate add-mutation family                          | 8 remaining add-mutation tests + the 2 `render_add_recovery_*` preview tests.                                    |
| 7   | refactor(recover): migrate remove-missing-mutation family               | 8 remove-missing tests + 3 `cmd_recover_remove_with_*` interrupted-Remove tests.                                 |
| 8   | refactor(recover): migrate remount-cycle family                         | 8 remaining remount-cycle tests. Promotes `RemountHarness` usage broadly.                                        |
| 9   | refactor(recover): migrate plan/pre-mount discovery family              | 5 `plan_recover_discovers_*` and `plan_recover_skips_pre_mount_discovery_*` tests.                               |
| 10  | refactor(recover): migrate inhibitor-failure family                     | 4 remaining tests using `FailingInhibitor` (1 already in PR 1 hard cases).                                       |
| 11  | refactor(recover): migrate dry-run + preview rendering tests            | 2 dry-run tests + 18 `plan_recover_dry_run_*` rendering tests.                                                   |
| 12  | refactor(recover): migrate end-to-end recover_* tests                   | 14 `recover_*` flows: bootstrap, mount-not-mounted, paused balance, by-id staleness, added_at carrying, etc.     |
| 13  | chore(recover): drop migrated test-mod helpers                          | Remove `recover_params`, `recover_params_with_inhibitor`, the local `MockFs`, the unused inline `_*Runner` chains that survived the per-test handler conversion. Audit unused journal/pool-state builders and drop the truly unused ones. |
| 14  | docs(plans): promote recover test-fixtures plan                         | Move `plans/wip/here-is-the-planner-buzzing-thompson.md` -> `plans/impl/2026-05-08-recover-test-fixtures.md` (or current date). |

The kernel-replace polling family (11 tests), by-id resolution family (2 tests),
and guidance helpers (9 tests) are not in this table -- they stay local
unchanged.

### Why PR-split (vs single PR)

- Recover.rs has 116 tests, ~94 to migrate. A single PR is reviewable but
  reviewer fatigue is real and the validation value of "design works on
  3 hard cases" is highest before bulk migration.
- PR 1 is small (~600-1200 lines diff: fixture seed + 3 tests) and merge-able fast.
- PR 2 is large but mechanical once PR 1 has shipped.
- If the team prefers, all 14 commits can be one PR -- they are
  independently green.

## Verification

Per-commit, before staging:

- `cargo test --manifest-path cli/Cargo.toml --lib recover::tests` -- full
  recover test module (fast, scoped).
- `cargo check --manifest-path cli/Cargo.toml --tests` -- typecheck all tests
  (catches fixture-API drift in unmigrated tests).

Hard-case single-test filters at PR 1 (one per migrated test, in commit order):

- `cargo test --manifest-path cli/Cargo.toml --lib recover::tests::plan_recover_discovers_add_targets_before_mount_planning`
- `cargo test --manifest-path cli/Cargo.toml --lib recover::tests::pool_mutation_inhibitor_failure_stops_before_destructive_replay`
- `cargo test --manifest-path cli/Cargo.toml --lib recover::tests::add_pool_mutation_replays_keyfile_enrollment_before_pool_add`

Polling-family single-test sanity (verify the stay-local family is
unaffected by fixture changes):

- `cargo test --manifest-path cli/Cargo.toml --lib recover::tests::wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished`

At sub-commit boundaries inside each PR (every 2-3 commits is enough):

- `just test-rust` -- full Rust unit-test suite, catches accidental
  fallout in non-recover modules (the `Config` field addition affects
  every existing scope fixture's `_inner` callers).

At end of PR 2:

- `just test-rust` (all unit tests).
- `just test-vm hello-world` -- fast smoke test that the workspace still
  builds in nix (fixtures shouldn't affect this, but the cost is low).

## Risks

**Fixture rigidity.** Building a `RecoveryPool`-style broad topology
installer would force one shape onto a behavior that varies per-family
(add-replay vs remove-missing-replay vs replace-replay). Mitigation:
this plan deliberately *does not* introduce one. Tests compose with
per-test `with_handler` closures. If a recurring closure shape emerges
(e.g., "live two-disk pool probe" appears in 8+ tests), introduce it as
a small `pub(crate) fn live_two_disk_probe(runner: MockRunner) -> MockRunner`
in `test_fixtures/recover.rs` after the bulk migration is in -- not
before. Premature consolidation is the canonical fixture-rigidity trap.

**Hidden state transitions.** The `MapperClosingRunner` mutates the
`StatefulMockFs` paths *and* a separate `closed` HashSet on every
`CryptsetupClose` / `CryptsetupLuksOpen`. The promoted `RemountHarness`
must preserve this dual mutation; missing one of the two on a refactor
silently breaks the close-then-reopen cycle. Mitigation: PR 1 commit 2
migrates a remount test to validate the promoted harness against real
test expectations before any other remount-cycle migrations.

**Request-order assertions.** Tests use `runner.requests()` to check
ordering (`luks_dump_text_request_count`, balance-vs-replace ordering,
etc.) and `RequestCountInhibitor::first_acquire_request_count()` to
check seam placement. The actual invariant is: `MockRunner::run` and
`MockRunner::run_with_stdin` (`cli/src/cmd.rs:1171-1199`) push the
request to `self.requests` *before* calling `self.dispatch`, so
recording is independent of whether a handler resolved the call or it
fell through to `with_output`. Migrated tests must continue to observe
the same shared `MockRunner` request log -- either via
`MockRunner::clone()` (cheap; clone shares the `Arc<Mutex<Vec<...>>>`)
or via `RemountHarness::requests()` for tests that wrap the runner.
Mitigation: don't introduce a fixture wrapper whose `run` records into
its own state instead of delegating to the inner `MockRunner`;
`RemountRunner` must call `self.inner.run(request)` for every
non-short-circuited request so the recorded ordering matches the
pre-migration log exactly. PR 1 commit 4 (RequestCountInhibitor) and
PR 1 commit 2 (RemountHarness) jointly validate this contract.

**Journal preservation.** Many error-path tests assert
`paths.pending_op_json().exists()` or `!paths.pool_json().exists()`
post-failure. The fixture intentionally does NOT abstract these
assertions -- they remain in test bodies. The fixture only owns the
*setup* of the temp `StatePaths`, not the post-condition shape.
Mitigation: keep `paths.pending_op_json().exists()` in tests as-is; do
not introduce `f.assert_journal_preserved()` style helpers (they'd
hide behavior).

**Pool.json preservation.** Same shape as journal preservation, with
the inverse assertion (`!paths.pool_json().exists()` for "pool.json
must NOT be written when X aborts"). Same mitigation.

**Passphrase-bytes drift.** Pre-migration recover tests write
`b"testpass"` to a local `NamedTempFile`; the fixture default
serves `b"test-passphrase"` (after newline strip). A test that
adopts the builder default but forgets to update its
`with_output_stdin(..., b"testpass".to_vec(), ...)` expectation
will hit a `stdin mismatch for {key}` panic from
`MockRunner::run_with_stdin` (`cli/src/cmd.rs:1193-1195`). Mitigation:
PR 1 commit 1 introduces `TEST_PASSPHRASE_BYTES` in
`cli/src/test_fixtures/shared.rs` and reuses it for both the
`empty_inner` file write and the recover stdin expectations, so the
constant and the on-disk bytes cannot drift. Bulk migrations audit
with `rg -n 'b"testpass"' cli/src/recover.rs` (the only acceptable
endpoint is zero matches outside rule-2 overrides) and either swap to
the shared constant or keep a local file via `.passphrase_file(...)`.
PR 1 commits 2-4 each touch this code path and validate the rule
before bulk migration begins.

**Dry-run / no-side-effect boundaries.** `recover_dry_run_does_not_acquire_sleep_inhibitor`
and the `plan_recover_dry_run_*` family assert specific commands NEVER
ran. The fixture must not stub commands that the dry-run path is
forbidden from issuing. Mitigation: dry-run tests use the smallest
possible `MockRunner` (just enough probes) and never call the broad-handler
closure shapes used by execution-path tests; review at PR 2 commit 11.

**`Config` field addition fallout.** Adding `config: Config` to
`PoolFixture` requires updating `empty_inner` and every scope-local
constructor (`live_one_disk`, `one_live_only`, `three_disk_devids_pinned`,
`two_disk_devids_pinned`, `three_disk_healthy`). Mitigation: PR 1 commit 1
covers all five constructors in one diff and runs `just test-rust` to
catch any cross-scope test breakage immediately.

**Plan-promotion sequencing.** The plan file currently sits at
`plans/wip/here-is-the-planner-buzzing-thompson.md`. After PR 2 ships,
commit 14 promotes it to `plans/impl/<date>-recover-test-fixtures.md`.
This is a separate commit so the implementation history doesn't carry
the random-name plan path forever.

## Critical Files

To be modified:

- `cli/src/test_fixtures.rs` -- add `mod recover;` + re-exports.
- `cli/src/test_fixtures/shared.rs` -- add `config: Config` field to
  `PoolFixture` + populate in `empty_inner`.
- `cli/src/test_fixtures/replace.rs`, `add.rs`, `remove.rs`,
  `remove_missing.rs` -- update each scope-local `PoolFixture`
  constructor to populate the new `config` field (one line each).
- `cli/src/test_fixtures/recover.rs` -- new. Hosts
  `RecoverParamsBuilder`, `RemountHarness`, `RemountFs`,
  `RemountRunner`, `recover_noop_inhibitor`, the
  `impl PoolFixture { recover_params(&self) -> RecoverParamsBuilder<'_> }`
  block, and any small helpers that surface during bulk migration.
- `cli/src/recover.rs` -- per-commit migrations of test bodies. The
  test-mod fakes that stay local (`ReplaceStatusSequenceRunner`,
  `MockByIdResolver`, `RequestCountInhibitor`, `FailingInhibitor`,
  `NoopInhibitor`, `passphrase`, `write_valid_keyfile`, the journal
  builders, the pool-state builders, the command-output helpers) keep
  their current locations until commit 13's cleanup audit.

To be referenced (read-only during implementation):

- `plans/impl/2026-05-07-mockrunner-handler-and-shared-test-fixtures.md` --
  source of truth for the migration model.
- `cli/src/cmd.rs` (`MockRunner` and helpers).
- The four prior scope-fixture modules as style references.

## Out of Scope

- Restructuring `recover.rs` into multiple files (e.g., per-family
  test modules). Orthogonal.
- Promoting recover-specific journal builders to a shared
  `journal_test_builders` module. Orthogonal.
- Eliminating `ReplaceStatusSequenceRunner` in favor of a `MockRunner`
  + handler-popping-from-queue pattern. The user's guidance preserves
  sharp local fakes for replace-status polling; honoring it.
- Eliminating `MockByIdResolver` for the same reason.
- Production code changes in `recover.rs`.
