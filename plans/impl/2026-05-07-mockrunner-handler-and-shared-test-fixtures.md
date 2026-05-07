# Plan: Consolidate per-test runner clutter via MockRunner extension + shared fixtures

**Status: Draft**

## Context

`cli/src/replace.rs` is 5715 lines, of which ~1500 are duplicated test scaffolding: 10 one-off `*Runner` structs (lines 2587-5493) plus dozens of identical 30-line `tempdir + config + pass + membership` setups. Each runner is a thin wrapper around the same pool-topology dispatch (`mapper -> backing dev`, `device -> LUKS UUID`, `btrfs filesystem show`) plus a single failure-injection point (replace fail, resize fail, close fail, etc.). The same pattern recurs across 5 sibling files (`add.rs`, `remove.rs`, `remove_missing.rs`, `recover.rs`, `doctor.rs`) for ~28 more runners.

Root cause: `cmd::MockRunner` (cli/src/cmd.rs:955) only matches via static `format!("{request:?}")` keys, so tests that need to dispatch by request field (e.g. "for any `CryptsetupLuksUuid`, return UUID by device") have to hand-roll a `CommandRunner` impl. Every author who hits this writes their own one-off runner and inlines the canonical pool-topology mocks.

Outcome: extend `MockRunner` with a closure-based fall-through handler so dynamic dispatch is expressible via the existing builder, and ship a `cli/src/test_fixtures.rs` module with the canonical pool-topology fixtures and tempdir/config builder. Migrate `replace.rs` as the first consumer to validate the design; leave the door open for siblings in follow-up PRs.

This is unreleased software (AGENTS.md "No backwards compatibility"), so we delete old scaffolding rather than deprecate it.

## Recommended approach

### A. Extend `cmd::MockRunner` with a closure-based handler

File: `cli/src/cmd.rs` (~80 LOC added).

Add an internal `handlers: Vec<Arc<dyn Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync>>` field on `MockRunner`. Default empty.

Add one new public method (requires a 1-3 line `///` per AGENTS.md doc-comment policy):

```rust
/// Closure-based fall-through handler so tests can dispatch by request
/// fields (e.g. mapper -> backing device) without enumerating every variant.
/// Handlers are tried in reverse registration order (last `with_handler` wins),
/// so generic fixture handlers can register first and per-test overrides last.
/// Returning `None` defers to the next handler, then to `with_output`.
pub fn with_handler<F>(mut self, handler: F) -> Self
where
    F: Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>>
        + Send + Sync + 'static,
{
    self.handlers.push(Arc::new(handler));
    self
}
```

Wire `MockRunner::run` and `run_with_stdin` to walk handlers in **reverse** registration order (most recently added first); on `Some(_)` return immediately; on all `None`, fall through to the existing static-key path. Request log captures every call regardless of who serviced it.

**Reverse-order rationale:** the canonical pool fixture (`ReplacementPool::install`) registers a generic topology handler first; per-test code then calls `with_handler` to override specific requests (e.g. `ReplaceKeyfileProbeRunner` at replace.rs:5429 makes disk3's `CryptsetupLuksUuid` return a non-LUKS error even though the fixture would otherwise resolve it). Forward-order semantics would let the generic fixture intercept the request before the per-test override could run.

**stdin-validation ordering (`run_with_stdin` only):** the existing contract (cmd.rs:1150-1152, pinned by `mock_runner_run_with_stdin_panics_on_stdin_mismatch_unchanged` at cmd.rs:1241) is that `run_with_stdin` panics on mismatch against any registered `stdin_expectations` for the request key. This is the line of defense for passphrase-sensitive tests; it must not regress. Specify that `run_with_stdin` validates `stdin_expectations` BEFORE handler dispatch -- the order is: log the request, assert any registered `stdin_expectations`, then walk handlers in reverse, then fall through to the static-key path. Handlers cannot bypass stdin assertions even on a successful return. (`run` has no stdin to validate, so the order is: log, walk handlers in reverse, fall through.)

**Side-effect post-processing** must apply to handler-returned outputs as well as static-key outputs. Today, `MockRunner::run` (cli/src/cmd.rs:1127-1136) creates the temp backup file on a successful `CryptsetupLuksHeaderBackup`; `backup_luks_header_to` (cli/src/luks.rs:447-481) then `set_permissions` + `durable_rename`s that file. If a handler returns a successful header-backup output, the post-processing must still create the file -- otherwise the chmod/rename fails with `ENOENT`. Refactor so the file-write block runs unconditionally after either an `Ok` handler output or a static-key match (i.e., apply post-processing in one place, after dispatch resolves).

Add focused unit tests in `cmd.rs::tests` proving:
1. Handler runs before static keys (registered handler intercepts a `with_output`-keyed request).
2. Handler returning `None` falls through to `with_output`.
3. Request log is populated regardless of who serviced the request.
4. Handler can stack with `with_output_sequence`.
5. **Last-handler-wins**: register handler A returning `Some(out_a)`, then handler B returning `Some(out_b)` for the same request; assert B's output is returned.
6. **Last-handler-with-fallthrough**: register handler A returning `Some(out_a)`; then handler B returning `None` for that request; assert A's output is returned.
7. **Header-backup side effect on handler success**: register a `with_handler` returning `Ok(exit_status: 0)` for `CryptsetupLuksHeaderBackup`; assert the backup file exists at the requested path after `run` returns.
8. **Header-backup side effect on handler failure**: register a `with_handler` returning `Ok(exit_status: 1)` for `CryptsetupLuksHeaderBackup`; assert the backup file does NOT exist after `run` returns (matches static-key behavior).
9. **stdin mismatch trumps handler success** (`#[should_panic(expected = "stdin mismatch")]`): register `with_output_stdin(req, b"secret", out)` and `with_handler` returning `Some(Ok(out_handler))` for the same `req`; call `run_with_stdin(&req, b"wrong")`; assert the runner still panics with `"stdin mismatch"` -- proves stdin validation runs before handler dispatch and a handler cannot mask a passphrase-bytes regression.

### B. New module `cli/src/test_fixtures.rs`

Gated `#[cfg(test)] pub(crate)`; registered in `cli/src/lib.rs` as `#[cfg(test)] pub(crate) mod test_fixtures;`. All items inside are `pub(crate)` and test-only -- no `///` doc-comment requirement (AGENTS.md:148).

```rust
// cli/src/test_fixtures.rs (~250 LOC)
pub(crate) fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput;

/// Generic Filesystem mock: paths-that-exist + mountinfo body + sysfs body.
pub(crate) struct MockFs { paths: Vec<String>, mountinfo: String, excl_op: String }
impl MockFs {
    pub(crate) fn storage(paths: Vec<String>) -> Self;            // mounted /mnt/storage
    pub(crate) fn unmounted(paths: Vec<String>) -> Self;          // no mountinfo entry
    pub(crate) fn with_excl_op(self, body: &str) -> Self;         // busy-op variant
}
impl crate::probe::Filesystem for MockFs { ... }

/// Canonical pool-topology mock-handler installer: mapper -> dev,
/// dev -> UUID, BtrfsFilesystemShow with optional state flipping.
pub(crate) struct ReplacementPool {
    pre_show: String,
    post_show: String,
    pre_usage_raw: String,             // BtrfsDeviceUsageRaw output pre-replace
    post_usage_raw: String,            // BtrfsDeviceUsageRaw output post-replace
    mapper_to_dev: HashMap<&'static str, &'static str>,
    dev_to_uuid: HashMap<&'static str, &'static str>,
    closed_mappers: HashSet<&'static str>,
}
impl ReplacementPool {
    pub(crate) fn two_disk_healthy() -> Self;          // disk1 + disk2 live
    pub(crate) fn one_live_one_missing() -> Self;      // disk1 live, devid 2 missing
    pub(crate) fn post_replace_two_disk(self) -> Self; // disk1 + disk3 live
    pub(crate) fn with_mapper_closed(mut self, mapper: &'static str) -> Self;
    pub(crate) fn install(self, runner: MockRunner,
                          replace_done: Arc<AtomicBool>) -> MockRunner;
    // install() pushes a single with_handler closure that resolves the full
    // canonical preflight + replace surface from the maps + replace_done flag:
    //   BtrfsFilesystemShow (state-flipping pre/post),
    //   BtrfsDeviceUsageRaw (state-flipping pre/post -- needed by
    //     preflight::probe_missing_devids on the missing path; cli/src/preflight.rs:305,
    //     called from resolve_replace_source at cli/src/replace.rs:1302),
    //   CryptsetupStatus (per-mapper open/closed via closed_mappers),
    //   CryptsetupLuksUuid (device -> UUID, dual /dev/vdX | by-id keys),
    //   CryptsetupLuksDumpText (LUKS2 stub),
    //   BtrfsBalanceStatus (no balance),
    //   BtrfsDeviceStatsJson (zero-counters default),
    //   CryptsetupTestPassphrase (success default).
    // `two_disk_healthy()`'s pre_usage_raw lists devid 1 + devid 2 with non-zero
    // device_size; `one_live_one_missing()`'s pre_usage_raw lists devid 1 +
    // <missing disk> with device_size=0 so probe_missing_devids returns [2].
    // post_usage_raw mirrors post_show: disk1 + disk3 healthy.
}

/// Bundled tempdirs + paths + config + passphrase + inhibitor for any
/// command that takes ReplaceParams/AddParams/RemoveParams.
pub(crate) struct PoolFixture {
    _state_tmp: TempDir, pub(crate) paths: StatePaths,
    _config_tmp: TempDir, pub(crate) config_path: PathBuf,
    pub(crate) pass_path: PathBuf,
    pub(crate) inhibitor: RecordingInhibitor,
}
impl PoolFixture {
    pub(crate) fn two_disk_healthy() -> Self;        // pool.json: disk1 + disk2
    pub(crate) fn one_live_one_missing() -> Self;    // pool.json: disk1 + disk2(devid=2)
    pub(crate) fn one_live_only() -> Self;           // pool.json: disk1(devid=1) only --
                                                     //   absent-old-name typo scenario for
                                                     //   cmd_replace_missing_path_rejects_old_name_absent_from_membership
                                                     //   (replace.rs:3416), which deliberately omits disk2.
    pub(crate) fn empty() -> Self;                   // no pool.json seeding
    pub(crate) fn with_keyfile_passphrase(self, kf: &Path) -> Self;
    pub(crate) fn replace_params(&self) -> ReplaceParamsBuilder<'_>;
}

/// Per-test ReplaceParams builder. Defaults match the most common test
/// shape (yes=true, dry_run=false, passphrase_file=Some(fixture pass_path),
/// passphrase_stdin=false, progress=Off, luks_format_extra_opts=&[],
/// missing_id=None, enroll_key_file=None). Every field overridable so tests
/// like cmd_replace_with_keyfile_orders_format_addkey_backup_open
/// (replace.rs:4478, custom luks_format_extra_opts) and
/// plan_replace_old_equals_new_aborts_before_any_probe (replace.rs:5283,
/// passphrase_file=None) round-trip identical ReplaceParams values.
pub(crate) struct ReplaceParamsBuilder<'a> { /* owns refs into the fixture */ }
impl<'a> ReplaceParamsBuilder<'a> {
    pub(crate) fn old(self, name: &'a str) -> Self;
    pub(crate) fn new(self, spec: &'a str) -> Self;
    pub(crate) fn missing_id(self, id: Option<u64>) -> Self;
    pub(crate) fn dry_run(self, dry_run: bool) -> Self;
    pub(crate) fn yes(self, yes: bool) -> Self;
    pub(crate) fn passphrase_stdin(self, on: bool) -> Self;
    pub(crate) fn passphrase_file(self, path: Option<&'a Path>) -> Self;
    pub(crate) fn enroll_key_file(self, path: Option<&'a Path>) -> Self;
    pub(crate) fn luks_format_extra_opts(self, opts: &'a [String]) -> Self;
    pub(crate) fn progress(self, p: ProgressOutput) -> Self;
    pub(crate) fn build(self) -> ReplaceParams<'a>;
}
```

`PoolFixture` subsumes the three near-clones at `add.rs:3207` (`add_test_setup`), `add.rs:3239` (`fresh_add_setup`), and `replace.rs:5504` (`plan_replace_fixture`). They get deleted in the follow-up PRs.

### C. Migrate `cli/src/replace.rs`

Delete in this order, sub-commit per group on a feature branch:

**Migration ordering principle:** prove the hard cases against the shared fixture API early, before the bulk migration. The hard cases are: (a) missing-path replace (exercises `BtrfsDeviceUsageRaw`), (b) raw replacement disk (exercises per-test handler overrides of the canonical topology), (c) state flip after replace (exercises `replace_done` integration through `ReplacementPool::install`), (d) header-backup side effects (exercises the unified post-processing). Sub-commits 3-6 below cover one of each before the bulk migration in 7-8.

| Sub-commit | Action | Validates |
|---|---|---|
| 1 | Land `MockRunner::with_handler` + unit tests (nine cases above) in `cmd.rs`. | The new primitive in isolation: dispatch order, fall-through, last-wins, header-backup post-processing, stdin-mismatch-trumps-handler |
| 2 | Land `cli/src/test_fixtures.rs` + register in `lib.rs`. No consumers yet. | Module compiles; no test changes |
| 3 | **Hard case (a) -- missing path.** Migrate `cmd_replace_missing_path_rejects_old_name_absent_from_membership` (line 3416) using `PoolFixture::one_live_only()` paired with `ReplacementPool::one_live_one_missing()` (membership lacks disk2 even though btrfs reports devid 2 missing -- this is the typo scenario the test pins). Delete `MissingPathReplaceRunner`. | `BtrfsDeviceUsageRaw` mock covers `probe_missing_devids`; absent-old-name shape preserved |
| 4 | **Hard case (b) -- raw disk override.** Migrate `plan_replace_keyfile_probe_failure_becomes_warn_notes` (5567) + `plan_replace_keyfile_asymmetry_suppresses_probe_failure_warning` (5613), delete `ReplaceKeyfileProbeRunner`. | Per-test `with_handler` overrides the fixture's disk3 LUKS UUID; reverse-order semantics |
| 5 | **Hard case (c) -- state flip.** Migrate `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize` (4212), delete `MissingPathSuccessRunner`. | `replace_done` flips `BtrfsFilesystemShow` and `BtrfsDeviceUsageRaw` between pre/post |
| 6 | **Hard case (d) -- header backup.** Migrate `replace_returns_enriched_error_when_post_format_backup_fails` (4592) + `cmd_replace_with_keyfile_orders_format_addkey_backup_open` (4478), delete `KeyfileOrderingReplaceRunner`. | Header-backup file-write post-processing fires for handler success; failure path leaves no file; `ReplaceParamsBuilder` carries `luks_format_extra_opts` |
| 7 | **Bulk migration -- live path.** Migrate `journal_survives_replace_failure` (2672), `mapper_open_true_verifies_but_does_not_open_new_disk_luks` (3975), `close_runs_before_resize_on_live_replace` (3033), `live_replace_old_close_failure_emits_warn_row` (3272), `wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal` (3861), `cmd_replace_rejects_old_equals_new` (2767), `dry_run_does_not_acquire_inhibitor` (2851). Delete `FailingReplaceRunner`, `RecordingReplaceRunner`, `ResizeFailingLoggingRunner`, `CloseFailingReplaceRunner`, `ClosedLuksWrongPassRunner`. | Bulk live-path coverage |
| 8 | **Bulk migration -- preview/plan.** Migrate `pool_json_persisted_when_missing_path_soft_balance_fails` (4792), `plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render` (4911), `plan_replace_missing_preview_has_no_notes_and_matches_legacy_step_render` (5015), `plan_replace_preflight_busy_op_becomes_info_note` (5173), `cmd_replace_old_equals_new_aborts_before_any_probe` (5662 -- KEEP `&PanicRunner` and `&PanicFilesystem`; only migrate temp-paths and params construction to `PoolFixture` + `ReplaceParamsBuilder`). Delete `MissingPathBalanceFailingRunner`. | Bulk plan/preview coverage; no-probe assertion preserved for the panic-boundary test |
| 9 | Delete now-unused locals: `mock_ok` (replace.rs:2550), `ReplaceMockFs` (2560), `ReplaceMockFsWithSysfs` (5123), `plan_replace_fixture` (5504), `PlanReplaceFixture` (5495). Confirm `cargo check --manifest-path cli/Cargo.toml --tests` is clean. | No dangling references |

### Sample migration (line 2672, currently ~75 lines)

```rust
#[test]
// Intent: pending-op.json survives when btrfs replace start fails.
//
// Why it exists: JournalGuard previously cleared the journal on any exit,
//   including error returns. After LUKS init on the new disk, a failed
//   btrfs replace would leave pool.json stale with no recovery path.
//
// Scenario: live replace, new disk already LUKS-open, btrfs replace start
//   fails (e.g. target too small). Journal must persist for recovery.
fn journal_survives_replace_failure() {
    let f = PoolFixture::two_disk_healthy();
    let replace_done = Arc::new(AtomicBool::new(false));
    let runner = ReplacementPool::two_disk_healthy()
        .install(MockRunner::default(), replace_done.clone())
        .with_handler(|req| match req {
            CmdRequest::BtrfsReplaceStart { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs replace start".into(),
                stdout: String::new(),
                stderr: "ERROR: target device is too small".into(),
                exit_status: 1,
            })),
            _ => None,
        });
    let fs = MockFs::storage(vec![
        "/dev/disk/by-id/virtio-disk3".into(),
        "/dev/mapper/braid-disk3".into(),
    ]);
    let result = cmd_replace(&runner, &fs, &f.replace_params()
        .old("disk2")
        .new("disk3=/dev/disk/by-id/virtio-disk3")
        .build());
    assert!(result.is_err(), "replace should fail when btrfs replace fails");
    assert!(journal::load_journal(&f.paths).unwrap().is_some(),
        "pending-op.json must survive error exit so braid recover can reconcile");
    assert_eq!(f.inhibitor.acquire_count(), 1,
        "sleep inhibitor must be acquired exactly once on the path through journal::write_journal");
}
```

The `// Intent / Why it exists / Scenario` preamble is preserved byte-for-byte. The body shrinks from ~75 lines to ~25.

## Critical files to modify

- `/Users/dan/Code/braid/cli/src/cmd.rs` -- add `handlers` field on `MockRunner`, the `with_handler` builder method (with doc comment), wire into `run` and `run_with_stdin`, add focused unit test in the existing `tests` mod.
- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- NEW. `mock_ok`, `MockFs` (replaces `ReplaceMockFs` + `ReplaceMockFsWithSysfs` + sibling clones), `ReplacementPool`, `PoolFixture`. All `pub(crate)`, all test-only.
- `/Users/dan/Code/braid/cli/src/lib.rs` -- add `#[cfg(test)] pub(crate) mod test_fixtures;`.
- `/Users/dan/Code/braid/cli/src/replace.rs` -- delete the 10 `*Runner` structs (lines 2587-5493 spanning the cluster), the local `mock_ok`, `ReplaceMockFs`, `ReplaceMockFsWithSysfs`, `plan_replace_fixture`, `PlanReplaceFixture`. Rewrite all migrated test bodies (sub-commits 3-8) to use `PoolFixture` + `MockRunner` + `ReplacementPool` + `MockFs`, EXCEPT preserve `&PanicRunner` and `&PanicFilesystem` in the four no-probe boundary tests: `plan_replace_old_equals_new_aborts_before_any_probe` (replace.rs:5261), `plan_replace_aborts_when_keyfile_missing_before_any_probe` (5317), `plan_replace_aborts_when_keyfile_is_directory_before_any_probe` (5373), and `cmd_replace_old_equals_new_aborts_before_any_probe` (5662). Those tests' assertion is precisely that no probe runs before validation; substituting a regular runner/fs would let an accidental pre-validation probe pass silently. Use `PoolFixture` + `ReplaceParamsBuilder` only for temp paths and params construction in those four. PanicRunner/PanicFilesystem live at replace.rs:1564/1580 (outside the deletion range) and stay. Preserve every `// Intent / Why it exists / Scenario` preamble byte-for-byte.

## Existing functions / utilities reused

- `cmd::MockRunner` (cli/src/cmd.rs:955) -- builder being extended, not replaced. All existing methods (`with_output`, `with_output_sequence`, `with_output_stdin`, `with_luks_dump_text_luks2`, `with_mapper_closed`, `with_mapper_open`) keep working unchanged.
- `cmd::CommandRunner` trait (cli/src/cmd.rs:829) -- target trait, no changes.
- `cmd::CmdRequest` / `cmd::RawCommandOutput` / `cmd::CmdError` (cli/src/cmd.rs:21, 822) -- types reused as-is.
- `probe::Filesystem` trait (cli/src/probe.rs:14) -- target trait for `MockFs`, no changes.
- `inhibit::RecordingInhibitor` -- bundled into `PoolFixture`.
- `state_paths::StatePaths::custom` -- bundled into `PoolFixture`.
- `membership::{PoolMembership, DiskMember, save_membership}` -- bundled into `PoolFixture::two_disk_healthy` / `one_live_one_missing`.

## Out of scope for this plan

- Migrating `add.rs`, `remove.rs`, `remove_missing.rs`, `recover.rs`, `doctor.rs` test mods. The new `PoolFixture` + `MockRunner::with_handler` make those migrations cheap, but they are deferred to separate follow-up PRs once `replace.rs` proves the design.
- Touching `cli/src/replace.rs` production code (lines 1-1545). This is a pure test-side refactor.

## Verification

End-to-end gate: `just test-rust` is green at every sub-commit boundary. Note: `test-rust` in the Justfile (Justfile:104) takes no arguments -- it runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard` as a fixed command. Filtered runs go through `cargo test` directly.

Per-sub-commit:

- **Sub-commit 1** (`MockRunner::with_handler`): `cargo test --manifest-path cli/Cargo.toml --lib cmd::tests` runs the nine new unit tests (dispatch order, fall-through, last-wins, request log, sequence stacking, two header-backup cases, stdin-mismatch-trumps-handler). `cargo check --manifest-path cli/Cargo.toml --tests` confirms no regressions. Then `just test-rust` for the full Rust gate.
- **Sub-commit 2** (`test_fixtures.rs` scaffolding): `cargo check --manifest-path cli/Cargo.toml --tests`. No test changes -- green is mechanical. Then `just test-rust`.
- **Sub-commits 3-8** (per-test migrations): for each migrated test, run that test by name -- e.g. `cargo test --manifest-path cli/Cargo.toml --lib replace::tests::journal_survives_replace_failure`. Then `cargo test --manifest-path cli/Cargo.toml --lib replace::tests` to confirm the rest of the file is still green. Then `just test-rust` for the full Rust gate at the sub-commit boundary. The migration is correct iff the test passes on identical assert messages.
- **Sub-commit 9** (cleanup): `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references; `just test-rust` full suite green.

Behavior-preservation check (manual, mechanical):

- For each migrated test, the `// Intent / Why it exists / Scenario` preamble must round-trip byte-for-byte. Validate via `git log -p cli/src/replace.rs` per sub-commit -- the diff for each migrated test should show body changes only, with the preamble lines unchanged.
- The set of asserts for each test must be unchanged. The migration touches setup code (runner construction, fs construction, params construction) -- nothing inside the asserts.
- The set of `cmd_replace` / `plan_replace` invocation arguments must be unchanged per test. Verify by `git diff` showing the same effective `ReplaceParams` field values before and after -- the `ReplaceParamsBuilder`'s defaults must produce the literal struct each migrated test originally constructed (especially `yes`, `passphrase_stdin`, `passphrase_file`, `progress`, `luks_format_extra_opts`). Spot-check by adding a temporary `dbg!()` of the built `ReplaceParams` in one test pre/post migration during sub-commit 6.

No new VM tests, no parser-fixture refresh, no production behavior change -- this is a test-side mechanical refactor. The existing test suite IS the verification.

## Branch and commit shape

Work on a feature branch (e.g. `refactor-test-mock-fixtures`). Each numbered sub-commit above is one git commit on that branch. PR opens once sub-commit 8 lands. Reviewer can walk the branch commit-by-commit; each commit is independently green.

Conventional Commits-style messages (lowercase first word per AGENTS.md):
- `refactor(cmd): add closure handler to MockRunner` (sub-commit 1)
- `refactor(test): add shared pool fixture and topology mock module` (sub-commit 2)
- `refactor(replace): migrate missing-path absent-name test to PoolFixture` (sub-commit 3)
- `refactor(replace): migrate keyfile-probe tests to handler overrides` (sub-commit 4)
- `refactor(replace): migrate state-flip soft-balance test to ReplacementPool` (sub-commit 5)
- `refactor(replace): migrate header-backup tests to unified post-processing` (sub-commit 6)
- `refactor(replace): migrate live-path tests to PoolFixture` (sub-commit 7)
- `refactor(replace): migrate plan/preview tests to PoolFixture` (sub-commit 8)
- `refactor(replace): drop unused per-test runner structs and local mocks` (sub-commit 9)
