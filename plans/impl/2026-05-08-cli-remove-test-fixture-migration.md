# cli/src/remove.rs test-fixture migration

## Context

The plan in `plans/impl/2026-05-07-mockrunner-handler-and-shared-test-fixtures.md`
introduced `MockRunner::with_handler` (cli/src/cmd.rs:1016-1052) plus the
`test_fixtures` module (`shared.rs` + per-command files for replace and add).
`cli/src/replace.rs` is fully migrated and is the canonical reference for what
a migrated consumer looks like (short test bodies, install-then-override).
`cli/src/add.rs` is mid-migration: its fixture module
`cli/src/test_fixtures/add.rs` is fully built out and is the right reference
for fixture *API* patterns (params builder, dynamic-fs, extension-impls on
`PoolFixture`), but the consumer file itself still carries multiple legacy
local runners (`AddTestRunner` at cli/src/add.rs:3088 and at lines 2282 / 2887,
`AddFullPathRunner` at cli/src/add.rs:3278, `SpyRunner` at cli/src/add.rs:2499,
`UnlockingAddRunner` at cli/src/add.rs:2750, `ClosedNoBtrfsRunner` at
cli/src/add.rs:2837, `RequestRecordingRunner` referenced at cli/src/add.rs:4178)
and is not a clean migrated-consumer reference. The goal here is to do for
`cli/src/remove.rs` what was done for `cli/src/replace.rs`: delete the per-test
`CommandRunner` impls and ad-hoc tempdir scaffolding, route every test through
the shared fixture machinery, and surface failure injection through per-test
handler overrides instead of bespoke runner structs.

remove.rs currently carries the largest local fixture surface in the crate
(roughly 14 test-only structs and ~1900 lines of test code at lines 682-2600).
Once migrated, the file should look like `cli/src/replace.rs` does today: short
test bodies that compose `PoolFixture` + a remove-scoped topology installer +
per-test `with_handler` overrides for failure injection.

## Current scaffolding inventory (grouped by behavior, not by struct)

All line ranges below refer to `cli/src/remove.rs` unless noted.

### A. Filesystems
- `MockFs` (694-718) -- canonical mounted /mnt/storage shape, `exclusive_operation = "none"`. Subsumed by `MockFs::storage(...)` in `cli/src/test_fixtures/shared.rs:41-48`.
- `MockFsWithExclop` (720-744) -- same mountinfo, parametrized `exclusive_operation`. Subsumed by `MockFs::storage(...).with_excl_op(body)` (`cli/src/test_fixtures/shared.rs:64-67`).

### B. Topology / request-recording runners (success-path)
- `RecordingRunner` (788-912) -- 2-disk topology, full success path: `BtrfsFilesystemShow`, `CryptsetupStatus`, `CryptsetupLuksUuid`, `BtrfsBalanceStatus`, `BtrfsDeviceUsageRaw`, `BtrfsFilesystemDfJson`, `BtrfsBalanceSingle`, `BtrfsDeviceRemove`, `CryptsetupClose`. Captures every `CmdRequest`. The capture surface is already provided by `MockRunner::requests()` (cli/src/cmd.rs:1074), so the `Arc<Mutex<Vec<...>>>` field is redundant.
- `RecordingRunner::with_device_remove_failure` (within 788-912) -- toggles `BtrfsDeviceRemove` to non-zero exit. Better expressed as a per-test `with_handler` override.
- `ThreeDiskRecordingRunner` (2024-2079) -- 3-disk variant of the above. Pure topology widening; the request log is again redundant.
- `ThreeDiskUsageOverrideRunner` + `UsageOverride` enum (1970-1977, 2085-2113) -- wraps the 3-disk runner to re-route `BtrfsDeviceUsageRaw` to spawn-error or parse-error. Per-test override.

### C. One-shot failure-injection runners around `check_eviction_space`
All seven sit between lines 1576-1955 and 2131-2202:
- `FailingUsageRunner` (1576-1598) -- usage exit=1.
- `UsageSpawnFailRunner` (1624-1638) -- any request returns `Err(MissingMock)`.
- `UsageParseFailRunner` (1665-1687) -- truncated usage stdout.
- `DfSpawnFailRunner` (1713-1748) -- usage ok, df errors.
- `DfParseFailRunner` (1774-1813) -- usage ok, df returns malformed JSON.
- `SurvivorMissingRunner` (1839-1875) -- usage stdout omits the survivor.
- `DfCommandFailedRunner` (1901-1941) -- usage ok, df exit=1.
- `UsageSpawnFailRunner` (re-declared 2131-2143) and `UsageParseShapeRunner` (2183-2202) for the 3->2 soft-warn lane.

Every one of these is a thin override over the same canonical 2-disk or 3-disk topology and is the textbook case for `with_handler`.

### D. Test-only helpers
- `setup_membership` (746-758) -- duplicates `PoolFixture::two_disk_healthy()` (`cli/src/test_fixtures/shared.rs:129-149`).
- `mock_out` (760-767) -- duplicates `mock_ok` (`cli/src/test_fixtures/shared.rs:16-23`).
- `test_target_device` (769-776) -- builds the canonical disk1 `PoolDevice` for `check_eviction_space` unit tests.
- `acked_disk` (778-786) -- builds `alert::AckedDisk` for the prune-stats integration test only.
- `RemoveConfirmDisk` (650-654) -- lightweight carrier for `format_remove_confirm` unit tests.
- `remove_present_work_plan_for_test` (620-644) -- `#[cfg(test)]` shortcut that synthesizes a `RemoveWorkPlan` so dry-run rendering tests skip the planner.
- `three_disk_pool_setup` (1985-2016) -- duplicates the shape of `PoolFixture::two_disk_healthy()`, just with three disks.

### E. Tests grouped by behavior
| Group | Tests | Approx loc | Difficulty |
| --- | --- | --- | --- |
| 1. 2-disk integration (request-order, state I/O, failure-mid-flow) | `two_to_one_remove_invokes_survivor_capacity_preflight` (931-996), `remove_two_disk_pool_balances_single_before_device_remove` (1083-1138), `cmd_remove_prunes_acked_stats_for_removed_devid` (1011-1068), `journal_survives_evict_failure` (1149-1203) | 235 | hard |
| 2. Dry-run rendering (no runner) | `dry_run_render_3disk_removal` (1209-1257), `dry_run_render_2disk_removal_includes_balance` (1263-1301) | 88 | easy |
| 3. Preflight exclusive-op | `remove_fails_fast_on_paused_balance` (1308-1351), `remove_warns_and_proceeds_on_active_op` (1359-1419) | 105 | moderate |
| 4. Confirm formatting (pure string) | `remove_confirm_normal` (1424-1446), `remove_confirm_degraded` (1449-1469), `remove_confirm_no_hw_info` (1472-1485) | 58 | trivial |
| 5. Post-commit error mapping | `save_membership_failure_classified_as_membership_persist` (1502-1522), `clear_journal_failure_classified_as_journal_clear` (1537-1563) | 48 | easy |
| 6. `check_eviction_space` 2->1 hard-reject | seven tests across 1575-1955 | 334 | moderate |
| 7. `check_eviction_space` 3->2 soft-warn | `check_eviction_space_ge2_soft_warns_on_usage_spawn_error` (2130-2165), `check_eviction_space_ge2_soft_warns_on_parse_shape_error` (2182-2220) | 75 | easy |
| 8. `plan_remove` soft-warn surfacing | `plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error` (2236-2295), `..._on_parse_error` (2308-2357) | 110 | hard |
| 9. Preview rendering | `plan_preview_renders_soft_warn_above_dry_run_steps` (2372-2414), `remove_warn_notes_render_canonical_bracketed_form` (2431-2444) | 57 | easy |
| 10. Preflight notes preservation | `plan_remove_preflight_busy_op_becomes_info_note` (2457-2522), `plan_remove_preserves_preflight_notes_on_disk_not_found` (2535-2599) | 131 | hard |

There is no "must not run past this boundary" test in remove.rs today -- nothing
is asserting against a `PanicRunner` / `PanicFilesystem`. So no sharp local
runner needs to be preserved on those grounds.

## Fixture design

### Reused as-is from `cli/src/test_fixtures/shared.rs`
- `mock_ok` (line 16) -- replaces `mock_out`.
- `MockFs::storage(...)` and `.with_excl_op(...)` (lines 41-67) -- replace `MockFs` and `MockFsWithExclop`.
- `PoolFixture::two_disk_healthy()` and `::empty()` (lines 129-188) -- cover all 2-disk membership + tempdir + config + passphrase + inhibitor needs.

### New, remove-scoped: `cli/src/test_fixtures/remove.rs`
A new sibling to `cli/src/test_fixtures/replace.rs` and `cli/src/test_fixtures/add.rs`. Public surface:

1. `PoolFixture::three_disk_healthy() -> PoolFixture` -- extension impl on the shared fixture, mirrors `two_disk_healthy` but seeds disk1+disk2+disk3 in pool.json and config. Lives here (not in shared) because remove is currently the only command that needs it; if add or replace ever do, promote it.
2. `RemovalPool` struct with:
   - `RemovalPool::two_disk()` -- canonical 2-disk steady-state (devid 1+2, mappers `braid-disk1`/`braid-disk2`, fixed LUKS UUIDs).
   - `RemovalPool::three_disk()` -- canonical 3-disk steady-state.
   - `RemovalPool::install(self, runner: MockRunner) -> MockRunner` -- registers a single broad handler covering every command emitted by the success path: `BtrfsFilesystemShow`, `CryptsetupStatus`, `CryptsetupLuksUuid`, `BtrfsBalanceStatus`, `BtrfsDeviceUsageRaw`, `BtrfsFilesystemDfJson`, `BtrfsBalanceSingle`, `BtrfsDeviceRemove`, `CryptsetupClose`. Mirrors `ReplacementPool::install` (cli/src/test_fixtures/replace.rs:150).
3. `RemoveParamsBuilder<'a>` -- fluent builder analogous to `ReplaceParamsBuilder` (cli/src/test_fixtures/replace.rs:273): `name`, `dry_run`, `yes`, `progress`, `build() -> RemoveParams`.
4. `PoolFixture::remove_params(&self) -> RemoveParamsBuilder` -- extension impl, mirrors `PoolFixture::replace_params` (cli/src/test_fixtures/replace.rs:248).
5. `target_device(name: &str) -> PoolDevice` -- replaces `test_target_device` for `check_eviction_space` unit tests; small public helper, not a topology.
6. Plain-text constants for the canonical "happy path" stdout the override tests need to perturb: `valid_two_disk_usage_stdout()`, `valid_three_disk_usage_stdout()`, `valid_two_disk_df_json()`, `valid_three_disk_df_json()`. Sharing the valid form from one place keeps each per-test override honest about what it is breaking.

`RemovalPool` deliberately does not expose a `with_device_remove_failure` knob.
Failure injection is the per-test override's job, registered after install so
the override wins. This keeps the topology a pure success-path builder, in line
with the principles from the original migration plan.

### Wiring
- Add `mod remove;` to `cli/src/test_fixtures.rs:30` alongside the existing `mod add; mod replace; mod shared;` declarations.
- All four submodules are private (cli/src/test_fixtures.rs:30-32), so anything the test bodies in `cli/src/remove.rs` (which sits at `crate::remove`) name has to come through the facade. Append a re-export line covering every remove-scoped helper used outside the fixture module:
  ```
  pub(crate) use remove::{
      RemovalPool, RemoveParamsBuilder, target_device,
      valid_two_disk_usage_stdout, valid_three_disk_usage_stdout,
      valid_two_disk_df_json, valid_three_disk_df_json,
  };
  ```
  This mirrors how `MockFs`, `PoolFixture`, and `mock_ok` are reached today (cli/src/test_fixtures.rs:35). `RemoveParamsBuilder` is included because tests that hold the builder in a let-binding need to name the type; chained-method call sites would not strictly require it, but the cost of including it is one identifier.
- `PoolFixture::three_disk_healthy()` and `PoolFixture::remove_params(&self)` are `impl` extensions on the existing `PoolFixture` type; they need `pub(crate)` visibility on the methods themselves but no facade re-export, since method lookup goes through the type (matching how `replace_params` works at cli/src/test_fixtures/replace.rs:248).

### What stays local in `cli/src/remove.rs`
- `RemoveConfirmDisk` (650-654) -- a 5-line carrier used only by three trivial format-string tests. Promoting it to the fixture module would add indirection without sharing.
- `acked_disk` (778-786) -- single-use helper for the prune-stats integration test. Keep until a second test needs it.
- `remove_present_work_plan_for_test` (620-644) -- a `#[cfg(test)]` constructor that exists specifically to bypass the planner so dry-run rendering tests stay focused. Belongs next to the rendering code under test.
- The five "behavior-not-topology" tests stay entirely local and untouched by the migration:
  - `remove_confirm_normal` (1424-1446), `remove_confirm_degraded` (1449-1469), `remove_confirm_no_hw_info` (1472-1485) -- pure formatter tests on `format_remove_confirm`. They use `RemoveConfirmDisk` only; no runner, no filesystem, no membership.
  - `save_membership_failure_classified_as_membership_persist` (1502-1522) -- deliberately writes a regular file at `tmp.path().join("not-a-dir")` (cli/src/remove.rs:1508) and then attempts `save_membership_to` through that non-directory path so `atomic_write` fails. `PoolFixture` provides a well-formed pool.json directory, which would defeat the failure injection.
  - `clear_journal_failure_classified_as_journal_clear` (1537-1563) -- creates a non-empty directory at `pending_op_json()` (cli/src/remove.rs:1546) so `fs::remove_file` errors with `EISDIR`/`ENOTEMPTY`. Same reasoning: well-formed fixture state would defeat the test.
  None of these five tests imports `mock_out`, `setup_membership`, `test_target_device`, or any of the local runners, so the cleanup commit does not need to touch their bodies.

These local items are not topology, not multi-test, and not duplicating shared
machinery, so they fall outside the migration scope.

### Hard cases that should migrate first

In order of fixture-shape risk (highest first), each should be its own
sub-commit so the shape is validated incrementally before bulk conversion:

1. `two_to_one_remove_invokes_survivor_capacity_preflight` (Group 1) -- proves the 2-disk topology covers the entire `cmd_remove` request sequence, including the order-sensitive preflight before the inhibitor and balance.
2. `plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error` and `..._on_parse_error` (Group 8) -- proves `RemovalPool::three_disk()` plus a per-test handler override on `BtrfsDeviceUsageRaw` produces the expected `PreviewNote::Warn` on the planner.
3. `journal_survives_evict_failure` (Group 1) -- proves failure injection at `BtrfsDeviceRemove` via a per-test `with_handler` override, not via a topology toggle. The production failure shape is a non-zero exit, not a runner error: the override must return `Some(Ok(RawCommandOutput { cmd: "btrfs device remove".into(), stdout: String::new(), stderr: "ERROR: error removing device".into(), exit_status: 1 }))`, matching the existing `RecordingRunner` behavior at cli/src/remove.rs:889-898. The "stops at the failure boundary" property is asserted by inspecting `MockRunner::requests()` after the call -- the captured sequence must end at the failed `BtrfsDeviceRemove` (no `CryptsetupClose` follows), since the broad `RemovalPool::install` handler would otherwise answer `CryptsetupClose` successfully.
4. `plan_remove_preflight_busy_op_becomes_info_note` and `plan_remove_preserves_preflight_notes_on_disk_not_found` (Group 10) -- proves `MockFs::with_excl_op` composes with `RemovalPool::three_disk()` and the planner's notes-on-error path survives.

If any of these four cases reveals a fixture gap (e.g. an unmocked
`BtrfsBalanceStatus` variant, a UUID/devid mismatch, or a request the topology
forgot), the gap is patched in `RemovalPool::install` before continuing.

## Sub-commit plan

Each row below is independently green: `cargo check --manifest-path cli/Cargo.toml --tests` and `cargo test --manifest-path cli/Cargo.toml --lib remove::tests` both pass at every commit.

| # | Title | Tests touched | What to verify |
| --- | --- | --- | --- |
| 1 | `test(remove): add RemovalPool, RemoveParamsBuilder, three_disk_healthy, target_device` -- create `cli/src/test_fixtures/remove.rs`, register in `cli/src/test_fixtures.rs`, no remove.rs change | 0 | `cargo check --tests`, `just test-rust` |
| 2 | `test(remove): migrate 2->1 request-order test to RemovalPool::two_disk()` | `two_to_one_remove_invokes_survivor_capacity_preflight` | `cargo test --lib remove::tests::two_to_one_remove_invokes_survivor_capacity_preflight` |
| 3 | `test(remove): migrate plan_remove soft-warn surfacing to RemovalPool::three_disk() + handler override` | `plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error`, `..._on_parse_error` | `cargo test --lib remove::tests::plan_remove_surfaces_soft_warn` |
| 4 | `test(remove): migrate device-remove failure-injection test to per-test handler` | `journal_survives_evict_failure` | `cargo test --lib remove::tests::journal_survives_evict_failure` |
| 5 | `test(remove): migrate preflight notes-preservation tests to MockFs::with_excl_op` | `plan_remove_preflight_busy_op_becomes_info_note`, `plan_remove_preserves_preflight_notes_on_disk_not_found` | `cargo test --lib remove::tests::plan_remove_pre` |
| 6 | `test(remove): migrate remaining 2-disk integration tests` | `remove_two_disk_pool_balances_single_before_device_remove`, `cmd_remove_prunes_acked_stats_for_removed_devid` | `cargo test --lib remove::tests` |
| 7 | `test(remove): migrate preflight + dry-run + preview-render tests` | `remove_fails_fast_on_paused_balance`, `remove_warns_and_proceeds_on_active_op`, `dry_run_render_3disk_removal`, `dry_run_render_2disk_removal_includes_balance`, `plan_preview_renders_soft_warn_above_dry_run_steps`, `remove_warn_notes_render_canonical_bracketed_form` | `cargo test --lib remove::tests` |
| 8 | `test(remove): migrate check_eviction_space tests to per-test handler overrides` | seven 2->1 hard-reject + two 3->2 soft-warn tests | `cargo test --lib remove::tests::check_eviction_space` |
| 9 | `refactor(remove): delete dead local test scaffolding` -- remove all 14 local structs, `mock_out`, `setup_membership`, `test_target_device`, `three_disk_pool_setup`, `UsageOverride` enum. The five local-only tests (three `remove_confirm_*` + two post-commit error mapping tests) are not migrated; this commit verifies they continue to compile and pass without any of the deleted helpers. | 0 (deletes only) | `cargo test --lib remove::tests`, `just test-rust` |
| 10 | `chore(plans): promote remove test-fixture migration plan` -- `plans/wip/draft-a-migration-plan-tingly-lynx.md` -> `plans/impl/2026-05-DD-cli-remove-test-fixture-migration.md` via `promote-plan` | 0 | none (admin) |

## Verification

Run at the end of every sub-commit:

```
cargo test --manifest-path cli/Cargo.toml --lib remove::tests
cargo check --manifest-path cli/Cargo.toml --tests
```

Single-test filters for the four hard cases (sub-commits 2-5):

```
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::two_to_one_remove_invokes_survivor_capacity_preflight
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::plan_remove_surfaces_soft_warn_as_preview_note_on_parse_error
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::journal_survives_evict_failure
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::plan_remove_preflight_busy_op_becomes_info_note
cargo test --manifest-path cli/Cargo.toml --lib remove::tests::plan_remove_preserves_preflight_notes_on_disk_not_found
```

Sub-commit-boundary gate:

```
just test-rust
```

`just test-vm` is not in scope -- nothing in this migration touches the NixOS module, fixtures, or hardware paths. If the fixture additions accidentally affect a non-test path, `cargo test --lib` will catch it.

## Risks and mitigations

- **Fixture rigidity.** `RemovalPool::install` registers one broad handler covering every command in the success path. If a future test needs a slightly different success path (e.g. a survivor at a different devid), the temptation is to add a knob to the topology. Mitigation: prefer per-test `with_handler` overrides over knobs; only widen `RemovalPool` when the same shape is needed by two or more tests.
- **Hidden behavior.** Pulling success-path mocks out of each test means the test body no longer shows what `BtrfsBalanceSingle` returns. Mitigation: keep failure injection inline in the test (per-test `with_handler`), so the perturbation is visible at the call site even if the baseline is not. Reuse `valid_*_usage_stdout()` / `valid_*_df_json()` so override tests can quote the exact bytes they are corrupting.
- **Request-order assertions.** Group 1 and Group 8 tests assert exact `MockRunner::requests()` sequences. If `RemovalPool::install` causes any extra command to be probed (e.g. an opportunistic `BtrfsFilesystemShow` not previously emitted), those assertions break for the wrong reason. Mitigation: design `RemovalPool::install` so it only mocks commands the production code path actually emits; do not pre-emptively mock "extra" commands. If a real code change later adds a probe, the test should fail on its own request-order assertion, not on `MissingMock`.
- **No-probe / no-side-effect boundaries.** remove.rs has no `PanicRunner`-style sharp boundary today, but the fixture must not silently relax one. Specifically, sub-commits 4 (`journal_survives_evict_failure`) and 5 (preflight notes) rely on the absence of post-failure commands. The broad `RemovalPool::install` handler covers `CryptsetupClose` and every other steady-state command, so an unmocked-after-failure command will *not* fall through to `MissingMock` -- it will quietly succeed via the topology handler. Mitigation: model the failure with the production shape (non-zero `exit_status`, not `Err(CmdError)`) and assert the boundary explicitly by inspecting `MockRunner::requests()` -- the captured sequence must end at the expected request and contain no later commands. If a future code change emits a command past the failure point, the request-list assertion fails, surfacing the regression even though the topology handler would have happily answered.
- **3-disk topology debt.** `RemovalPool::three_disk()` and `PoolFixture::three_disk_healthy()` are introduced solely for remove.rs. If they leak into add.rs or replace.rs later, that is a signal to promote `three_disk_healthy` from `cli/src/test_fixtures/remove.rs` into `cli/src/test_fixtures/shared.rs:188` next to the existing two-disk and missing variants.
- **`MockRunner::requests()` correctness.** The migration replaces `RecordingRunner`'s `Arc<Mutex<Vec<CmdRequest>>>` log with `MockRunner::requests()` (cli/src/cmd.rs:1074). It is built in and proven by the replace migration, but the first migrated request-order test (sub-commit 2) is the validation point; if its assertions don't line up with the captured sequence, fix the topology before continuing.
- **Diff churn.** The migration deletes ~400 lines of local scaffolding and rewrites ~1500 lines of test bodies. Sub-commit 9 collects all deletions in one place so reviewers can see the dead code go away in isolation, but the per-test bodies in sub-commits 2-8 will still be heavy diffs. Mitigation: preserve the `// Intent / Why it exists / Scenario` preambles byte-for-byte (per the project's test convention), so reviewers can diff body-only.

## Critical files

- `cli/src/remove.rs` -- the file under migration; tests at 682-2600.
- `cli/src/test_fixtures.rs` -- facade module; needs `mod remove;` + re-export.
- `cli/src/test_fixtures/shared.rs` -- read-only here; `MockFs`, `mock_ok`, `PoolFixture` are reused as-is.
- `cli/src/test_fixtures/remove.rs` -- new file; `RemovalPool`, `RemoveParamsBuilder`, `three_disk_healthy`, `target_device`, `valid_*` helpers.
- `cli/src/test_fixtures/replace.rs` -- reference for `ReplacementPool`, `ReplaceParamsBuilder`, install-then-override pattern; pair with `cli/src/replace.rs` for migrated test-body shape.
- `cli/src/test_fixtures/add.rs` -- reference for the per-command extension-impl pattern (`PoolFixture::live_one_disk()`, `PoolFixture::add_params()`). Note that `cli/src/add.rs` itself is mid-migration (legacy `AddTestRunner` etc. at lines 2282 / 2887 / 3088) and is *not* a migrated-consumer reference -- consult `cli/src/replace.rs` for that.
- `cli/src/cmd.rs` -- read-only; `MockRunner` (960-1075), `with_handler` (1016-1027), `dispatch` (1031-1052), `requests()` (1074).
- `plans/impl/2026-05-07-mockrunner-handler-and-shared-test-fixtures.md` -- conventions and lessons learned from the original migration.
