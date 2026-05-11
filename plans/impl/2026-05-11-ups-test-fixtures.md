# Plan: Migrate `cli/src/ups.rs` test scaffolding to `test_fixtures::ups`

**Status: Draft**

## Context

`cli/src/ups.rs::tests` is the only command-tests module in the crate that
still defines its own `(CmdRequest, RawCommandOutput)` bodies and an
on-disk `config.json` writer inline. Every other command (`scrub`,
`status`, `mount`, `lock`, `ack`, `monitor`, `idle`, `discover`, ...) has
been migrated to a flat `test_fixtures::<name>` module that ships the
small set of helpers each tests file repeated.

The goal here is the same cleanup, scoped to ups: lift the repeated UPS
test setup out of `ups.rs` while keeping formatting, snapshot, and
parser-focused tests easy to read at the call site. The migration is
intentionally narrow -- no broad runner, no params builder, no topology
installer. It removes boilerplate from command-boundary tests; it does
not change test meaning.

## Goals

Simplify `cli/src/ups.rs::tests` by moving the small set of UPS-shaped
helpers it re-defines into a focused `cli/src/test_fixtures/ups.rs`
module. Preserve the contracts that make those tests valuable:

### `query_ups`

- Non-zero `upsc` exit maps to `UpsQueryError::QueryFailed { exit_code, stderr }`,
  with `stderr` trimmed.
- Runner-level failure (`CmdError::MissingMock`, spawn error,
  signal-killed child) maps to `UpsQueryError::InvocationFailed(_)`.
- Healthy `ups.status: OL\nbattery.charge: 100\n` parses cleanly:
  `UpsStatusFlag::Ol` present in `status_flags`,
  `battery.charge_pct == Some(100)`.

### `cmd_ups_status`

- Invocation failure surfaces as `UpsError::QueryFailed { detail }` with
  `detail.starts_with("invocation failed: ")`, and `err.to_string()`
  starts with `"upsc query failed: invocation failed: "`.
- Non-zero `upsc` exit surfaces as `UpsError::QueryFailed { detail }`
  with `detail.starts_with("exit 1: ")` and the full `Display` exactly
  `"upsc query failed: exit 1: Error: Connection failure: Connection refused"`.
- `--json` query-failure path returns `UpsError::QueryFailedJsonReported`
  so the CLI shell skips the human-readable stderr line.

### JSON sentinel + snapshot envelope

- `JsonReport::Error(ErrorReport::NotEnabled)` round-trips with `"error"`
  + `"ups_not_enabled"`.
- `JsonReport::Error(ErrorReport::QueryFailed { detail })` round-trips
  with `"query_failed"` and embeds the verbatim `detail`.
- All four `snapshot_human_*` insta snapshots, both `snapshot_json_*`
  sentinels, and the four `json_*_fixture_has_expected_shape` structural
  assertions stay stable.

This is a test-side refactor only. Do not change `query_ups`,
`cmd_ups_status`, `emit_query_failed`, `emit_json`, `format_human`,
`format_status`, `format_runtime`, `JsonReport`, `ErrorReport`,
`UpsError`, or `UpsQueryError`. Do not change any captured
`cli/tests/fixtures/nixos-25.11/upsc/*.txt`.

## Current-State Inventory

`cli/src/ups.rs` is 757 lines. The `#[cfg(test)] mod tests` block runs
from line 234 to 757 (25 tests plus roughly 25 lines of local
scaffolding plus two macro definitions).

### Local helpers

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `write_ups_config(&TempDir, &str) -> PathBuf` | 242-250 | Writes a `config.json` with a `mount_point` and a `ups: { enable: true, name: "<name>" }` block into the tempdir; returns the path so `cmd_ups_status` can call `config_read`. | Promote as `ups_write_config(&TempDir, &str) -> PathBuf`. The two command-boundary tests both call it; the format is byte-identical. The doctor fixture's `config_with_ups_enabled()` returns a `&'static str` JSON body (for `DoctorContext::for_test_ups`), not a path, so it is not a substitute. |
| `parse_fixture(stdout: &str) -> UpscOutput` | 583-585 | One-line shim around `crate::parse::parse_upsc(stdout)` used by each fixture-backed snapshot test. | Keep **local**. Promoting it adds zero readability -- the call site is already `crate::parse::parse_upsc(...)`. Removing this shim is a separate one-line edit, out of scope. |
| `snap!` / `snap_json!` macros | 587-601 | Wrap `insta::assert_snapshot!` / `assert_json_snapshot!` with `prepend_module_to_snapshot => false`. | Keep **local**. A `macro_rules!` macro can be scoped across modules with `pub(crate) use module::macro_name;` (no `#[macro_export]` required), so promotion is mechanically cheap. But six call sites do not justify centralizing them: the per-call-site savings would be zero, the macros only suppress an insta path prefix, and the macros never need to vary by scope. Promotion would add a facade entry without removing a single line at any call site. |

### Inline bodies the migration could touch

| # | Test | Lines | Inline body | Migration call |
|---:|---|---:|---|---|
| 1 | `query_ups_returns_query_failed_on_non_zero_exit` | 428-449 | `with_output(UpscQuery { name: "ups".into() }, RawCommandOutput { cmd: "upsc ups", stdout: "", stderr: "Error: Connection failure: Connection refused\n", exit_status: 1 })` | Promote as `ups_query_connection_refused_with_newline() -> (CmdRequest, RawCommandOutput)`. |
| 2 | `query_ups_returns_invocation_failed_on_missing_mock` | 459-469 | `MockRunner::default()` with no seed -- the missing seed IS the test setup. | Keep **inline**. Per the user's explicit constraint: do not hide the missing-mock test behind a helper that seeds `UpscQuery`. |
| 3 | `query_ups_returns_ok_on_healthy_output` | 478-494 | `with_output(UpscQuery { name: "ups".into() }, RawCommandOutput { cmd: "upsc ups", stdout: "ups.status: OL\nbattery.charge: 100\n", stderr: "", exit_status: 0 })` | Promote as `ups_query_healthy_minimal() -> (CmdRequest, RawCommandOutput)`. Body is the minimal OL + 100% slice the test parses against. |
| 4 | `cmd_ups_status_invocation_failure_surfaces_typed_error` | 502-519 | `MockRunner::default()` + `write_ups_config(&dir, "ups")` -- the runner stays empty on purpose; the config setup is the boilerplate. | Migrate only `write_ups_config` to `ups_write_config`. Keep the empty `MockRunner::default()` inline. |
| 5 | `cmd_ups_status_non_zero_exit_is_query_failed` | 527-552 | `with_output(UpscQuery { name: "ups".into() }, RawCommandOutput { cmd: "upsc ups", stdout: "", stderr: "Error: Connection failure: Connection refused", exit_status: 1 })` plus `write_ups_config`. | Promote as `ups_query_connection_refused_no_newline() -> (CmdRequest, RawCommandOutput)`. Note: test 1's stderr ends in `\n`; this one does not. Both must keep their exact shapes -- see "Why two connection-refused helpers" below. |

### Tests not affected by migration

| Group | Tests | Reason |
|---|---|---|
| Pure formatters | `format_status_ol`, `format_status_ob_lb_sorted`, `format_status_empty_is_unknown`, `format_runtime_splits_on_hour_boundary` | No runner, no config, no fixture. Pure value-in / value-out. User constraint: do not migrate pure formatting tests. |
| Pure JSON serialization | `json_output_has_status_and_battery_keys`, `json_not_enabled_has_sentinel_error`, `json_query_failed_has_sentinel_error_and_detail` | Build a `JsonReport` literal in-place and serialize. No runner, no config. |
| JSON error-emitter branch | `emit_query_failed_json_returns_already_reported` | Calls `emit_query_failed(true, "exit 1: dummy".into())` and asserts the returned error matches `UpsError::QueryFailedJsonReported`. No `JsonReport` literal, no runner, no config -- the branch under test is the variant choice in `--json` mode. |
| Pure format-human shape | `format_human_renders_dash_for_missing_optional_fields`, `format_human_load_omits_estimated_when_nominal_watts_missing` | Build a synthetic `UpscOutput` literal in-place to drive the "no value" arms. |
| Fixture-backed snapshot pairs | `snapshot_human_online`, `json_online_fixture_has_expected_shape`, `snapshot_human_onbattery`, `json_onbattery_fixture_has_expected_shape`, `snapshot_human_lowbattery`, `json_lowbattery_fixture_has_expected_shape`, `snapshot_human_replace_battery`, `json_replace_battery_fixture_has_expected_shape` | Each test calls `include_str!("../tests/fixtures/nixos-25.11/upsc/<name>.txt")` then `parse_fixture(...)` then either `snap!(format_human(...))` or structural `serde_json::Value` assertions. User constraint: do not turn snapshot tests into fixture indirection unless the helper is trivial and keeps the captured fixture path obvious. A wrapper would hide which `.txt` file each test reads -- the captured-fixture path is the most useful piece of context when an insta diff comes in. **Decision:** leave all eight unchanged. |
| Snapshot of constructed JSON | `snapshot_json_query_failed`, `snapshot_json_not_enabled` | Build the `JsonReport::Error(...)` literal in-place. Bodies are five lines each. |

### Why two connection-refused helpers

Tests 1 and 5 currently use stderr bodies that differ by one byte
(`"...refused\n"` vs `"...refused"`).

- Test 1 (`query_ups_returns_query_failed_on_non_zero_exit`) seeds
  stderr WITH a trailing newline. The assertion checks that the stored
  `UpsQueryError::QueryFailed { stderr }` value is the trimmed
  `"Error: Connection failure: Connection refused"`. The trailing
  newline is what makes that test a trimming proof at the `query_ups`
  layer.
- Test 5 (`cmd_ups_status_non_zero_exit_is_query_failed`) seeds stderr
  WITHOUT a trailing newline. The full Display assertion pins
  `"upsc query failed: exit 1: Error: Connection failure: Connection refused"`,
  which `cmd_ups_status` builds via
  `format!("exit {exit_code}: {stderr}")` over whatever `query_ups`
  returned. The current body is already trimmed, so test 5 does not
  re-prove trimming -- it pins the command-layer Display format and
  deliberately keeps its dependency surface narrow (no reliance on
  `query_ups`'s trim behavior).

A single shared helper (either body shape) would force a body change
at one of the two call sites. Ship two helpers so each test keeps the
exact local body it has today (byte-identical migration). Names
document the trailing-newline shape (`_with_newline` / `_no_newline`),
not the stderr substring.

## Existing fixture modules

Each candidate for reuse was evaluated against the constraint that the
output shape must be semantically correct for `ups.rs`'s tests, not
merely parse-compatible.

- **`shared::mock_ok`.** Returns `RawCommandOutput { cmd, stdout, stderr: "", exit_status: 0 }`. Use it **internally** in the healthy helper. No facade re-export required.
- **`doctor::config_with_ups_enabled() -> &'static str`** and **`doctor::ups_ctx`.** These exist for `DoctorContext::for_test_ups`-driven doctor tests; the value is a JSON body string passed to a doctor-only constructor, not a file path. They cannot replace `write_ups_config`, which puts the same JSON body **on disk** so `cmd_ups_status` can call `config_read(&path)`. **Decision:** keep `ups_write_config` self-contained.
- **`doctor::UpscSpawnFailureRunner`.** Doctor-shaped strict runner that returns `Err(CmdError::Failed)` on `UpscQuery` and `Err(CmdError::MissingMock)` on everything else. The `ups.rs` invocation-failure test uses bare `MockRunner::default()` to trigger `CmdError::MissingMock` directly -- a different shape. User constraint also explicitly forbids hiding the missing-mock test behind a helper. **Decision:** do not reuse.
- **`shared::MockFs` / `PoolFixture` / `StatePaths`.** UPS tests touch neither the `Filesystem` trait nor pool state. Skip.
- **`status::status_mp` / `mount::test_config` / `lock::lock_test_config`.** Each writes a fixture-scope `Config` for its own command; none writes `config.json` to disk in the shape `cmd_ups_status` expects. **Decision:** do not reuse.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/ups.rs` as a flat ups-scoped module.
Register it in `cli/src/test_fixtures.rs` with `mod ups;` and facade
re-exports.

Do not create a `UpsTopology` installer, a healthy-runner factory, a
params builder, or a broad ups runner. The ups tests are deliberately
narrow and the missing-mock test is the only proof that runner failures
map to `InvocationFailed`. A multi-test runner would silently resolve
that probe and flip the test's meaning.

### Public fixture surface

```rust
// On-disk config writer for cmd_ups_status tests. Writes
// {"mount_point":"/mnt/storage","ups":{"enable":true,"name":"<name>"}}
// into <dir>/config.json and returns the path.
pub(crate) fn ups_write_config(dir: &TempDir, name: &str) -> PathBuf;

// (CmdRequest, RawCommandOutput) pair for the healthy minimal slice
// that query_ups parses: OL flag + 100% battery. Stdout body matches
// the local body byte-for-byte; stderr empty; exit 0.
pub(crate) fn ups_query_healthy_minimal() -> (CmdRequest, RawCommandOutput);

// (CmdRequest, RawCommandOutput) pair used by
// query_ups_returns_query_failed_on_non_zero_exit: exit 1, empty
// stdout, stderr WITH the trailing newline ("...refused\n"). The
// trailing newline is what lets that test prove `query_ups` trims
// before storing.
pub(crate) fn ups_query_connection_refused_with_newline() -> (CmdRequest, RawCommandOutput);

// (CmdRequest, RawCommandOutput) pair used by
// cmd_ups_status_non_zero_exit_is_query_failed: exit 1, empty stdout,
// stderr WITHOUT a trailing newline ("...refused"). Byte-identical to
// the current local body so the command-layer Display assertion stays
// stable and the test keeps its narrow dependency surface (no reliance
// on `query_ups`'s trim behavior).
pub(crate) fn ups_query_connection_refused_no_newline() -> (CmdRequest, RawCommandOutput);
```

Implementation notes:

- All helpers are `pub(crate)` and test-only (`#[cfg(test)]`).
- `ups_write_config` is the byte-identical body of the local helper.
- All three pair helpers use `CmdRequest::UpscQuery { name: "ups".into() }`. No test exists that varies the name, so a parameterised
  `ups_query_*(name: &str)` is unnecessary.
- `ups_query_healthy_minimal` uses `super::shared::mock_ok("upsc ups", "ups.status: OL\nbattery.charge: 100\n")` for the response side.
- The two connection-refused helpers construct `RawCommandOutput` inline (not via `mock_ok`, because exit is 1 and stderr is non-empty). Names document the trailing-newline shape, not the stderr substring.
- No helper called `ups_query_failed` exists in the surface. That name would imply the body is a reusable canonical "any query failure" body, when in fact each helper preserves a byte-identical migration of an existing local body: test 1's body has the trailing newline that makes it a trimming proof; test 5's body omits it and pins the command-layer Display format.

### Facade exports

Add a ups block to `cli/src/test_fixtures.rs`:

```rust
mod ups;

#[allow(unused_imports)]
pub(crate) use ups::{
    ups_query_connection_refused_no_newline, ups_query_connection_refused_with_newline,
    ups_query_healthy_minimal, ups_write_config,
};
```

Update all three module-doc inventories in `cli/src/test_fixtures.rs`
so they stay internally consistent after `mod ups;` lands:

1. **Top abstract** (the "Test-only shared fixtures for `replace`,
   `add`, ...`scrub`, and `discover`." sentence at the head of the
   file): add `ups` to the comma list.
2. **Per-module bullet split** (the list where each existing fixture
   family has its own `* <name> -- ...` entry): add the new bullet
   below.
3. **Layout sentence** (the "`replace`, `add`, ...`scrub`, and
   `discover` hold their per-scope topologies, builders, and helpers."
   line at the bottom of the doc comment): add `ups` to the comma
   list.

The new bullet for step 2:

> `ups` -- flat ups-shaped helpers for `cli/src/ups.rs::tests`:
> `ups_write_config` (on-disk `config.json` writer for `cmd_ups_status`
> tests) and three `(CmdRequest, RawCommandOutput)` pair factories
> (healthy OL+100%, plus two daemon-down variants -- with and without
> a trailing stderr newline -- that match the current local bodies
> byte-identically so the trim-proof and command-layer Display tests
> each keep the body they have today). Ships flat because the test
> surface is small and tightly scoped to the runner / config boundary.
> No broad runner helper: the missing-mock test deliberately uses
> `MockRunner::default()` to trigger `CmdError::MissingMock`, and a
> multi-test runner would mask that proof. The `ups_` prefix avoids
> facade collisions with the `doctor::config_with_ups_*` family.

### Why `ups_` is the prefix

1. **Facade collision avoidance.** The doctor scope exports three
   `config_with_ups_*` constants used by doctor's UPS-daemon tests.
   Promoting an unprefixed `write_config` from `ups.rs` would imply
   equivalence with those constants and invite a future refactor to
   collapse them; they are not equivalent (`&'static str` body vs
   on-disk write).
2. **Staged-migration safety.** During the migration sub-commit, the
   local `write_ups_config` survives until the same sub-commit deletes
   it. The promoted name `ups_write_config` is distinct enough that the
   import does not collide with the local identifier.

### What stays local

- The `snap!` and `snap_json!` macros stay local. `pub(crate) use`
  can scope a `macro_rules!` macro across modules without
  `#[macro_export]`, so promotion is mechanically cheap -- but the six
  call sites do not justify centralizing them. The macros only
  suppress an insta path prefix and never need to vary by scope, so
  promotion would add a facade entry without saving a single line at
  any call site.
- The `parse_fixture` one-line shim stays local. Promoting it would not
  change any call-site lines.
- All eight fixture-backed snapshot tests stay structurally unchanged.
  The `include_str!` line with the captured fixture path is the most
  useful piece of context when an insta diff comes in.
- The two `format_human_*` synthetic-input tests stay structurally
  unchanged. The `UpscOutput { ... }` literal IS the test's setup.
- The four pure-formatter tests stay structurally unchanged.
- The three pure-JSON `JsonReport`-literal tests and the
  `emit_query_failed_json_returns_already_reported` emitter-branch test
  stay structurally unchanged.
- The two snapshot-of-constructed-JSON tests stay structurally
  unchanged.
- `query_ups_returns_invocation_failed_on_missing_mock` keeps its bare
  `MockRunner::default()` inline. The empty runner IS the test's setup.
- The empty `MockRunner::default()` in
  `cmd_ups_status_invocation_failure_surfaces_typed_error` stays inline
  for the same reason -- only the config writer is migrated.
- Per-test `// Intent / Why / Scenario` preambles stay local.

### What does not go in `shared`

No new `shared` helper is required. UPS fixtures are tied to
`CmdRequest::UpscQuery` and to a specific on-disk `config.json` shape
with a `ups` block -- both ups-scoped concerns. `mock_ok` already
covers the only cross-command primitive (exit-0 builder).

## Staged Migration

Two sub-commits. Each must compile and keep

```sh
cargo test --manifest-path cli/Cargo.toml --lib ups::tests
just test-rust
```

green at each boundary. The promoted names (`ups_write_config`,
`ups_query_*`) do not collide with the locals (`write_ups_config` and
the inline `with_output(...)` chains), so sub-commit 2 can migrate its
call sites cleanly and delete the obsolete local in the same commit.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(ups): add ups fixture module` | Add `cli/src/test_fixtures/ups.rs` with `ups_write_config`, `ups_query_healthy_minimal`, `ups_query_connection_refused_with_newline`, `ups_query_connection_refused_no_newline`. Register `mod ups;` and the four facade re-exports in `cli/src/test_fixtures.rs`. Update all three module-doc inventories in `cli/src/test_fixtures.rs` so they stay internally consistent after `mod ups;` lands: (a) the top abstract at the head of the file (the "Test-only shared fixtures for ..." sentence) -- add `ups` to the comma list; (b) the per-module bullet split (where each existing fixture family is described) -- insert the new `ups` bullet shown in the "Facade exports" section above; (c) the Layout sentence at the bottom of the doc comment (which enumerates every per-scope module after `shared`) -- add `ups` to the comma list. No `ups.rs` call sites change yet; no locals are deleted yet. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib ups::tests`; `just test-rust` |
| 2 | `test(ups): migrate runner-integration tests to ups fixtures` | In `ups.rs::tests`: import `ups_query_connection_refused_no_newline`, `ups_query_connection_refused_with_newline`, `ups_query_healthy_minimal`, `ups_write_config` from the facade. Migrate the four runner-integration tests: `query_ups_returns_query_failed_on_non_zero_exit` (uses `_with_newline`), `query_ups_returns_ok_on_healthy_output` (uses healthy minimal), `cmd_ups_status_invocation_failure_surfaces_typed_error` (uses `ups_write_config`; `MockRunner::default()` stays inline), `cmd_ups_status_non_zero_exit_is_query_failed` (uses `_no_newline` and `ups_write_config`). `query_ups_returns_invocation_failed_on_missing_mock` stays untouched -- the bare `MockRunner::default()` IS the test setup. Delete local `fn write_ups_config` in the same commit. | `cargo check --manifest-path cli/Cargo.toml --tests`; run each migrated test by name; `cargo test --manifest-path cli/Cargo.toml --lib ups::tests`; `just test-rust` |

There is no separate final-cleanup commit. Sub-commit 2 deletes the
only local helper (`write_ups_config`) in-place. After sub-commit 2,
`ups.rs::tests` contains 25 tests (all behaving identically), the two
macros, the `parse_fixture` shim, and the inline scaffolding for the
missing-mock test, the invocation-failure runner, the two
`format_human_*` synthetic-`UpscOutput` literals, the three pure-JSON
`JsonReport` literals, the `emit_query_failed` JSON-mode-variant
branch (no literal -- direct emitter call), the two `snapshot_json_*`
`JsonReport` literals, and the four fixture-backed snapshot pairs
with their `include_str!` lines preserved.

## Risks

- **Hiding the missing-mock contract behind a helper.** A future
  contributor might add `ups_query_no_seed() -> MockRunner` for symmetry
  with the other three pair helpers. Mitigation: the plan does not ship
  that helper, and the module doc comment in `test_fixtures::ups` calls
  out that the missing-mock test deliberately uses bare
  `MockRunner::default()` so the proof remains observable.
- **Collapsing the two connection-refused helpers into one.** The
  `_with_newline` body is what makes test 1 a trimming proof; the
  `_no_newline` body is byte-identical to test 5's current local
  body and keeps test 5's dependency surface narrow (command-layer
  Display only, no implicit reliance on `query_ups`'s trim). Either
  collapse direction would force a body change at one of the two
  sites. Mitigation: ship two helpers with names that document the
  trailing-newline shape, not the stderr substring.
- **Hiding the captured-fixture path behind a `parse_fixture` wrapper.**
  Mitigation: the plan does not migrate the eight fixture-backed
  snapshot tests. Each test still spells out its
  `include_str!("../tests/fixtures/nixos-25.11/upsc/<name>.txt")` at the
  call site.
- **Migrating pure formatter tests.** User constraint is explicit.
  Mitigation: the plan leaves all four `format_status_*`, the one
  `format_runtime_*`, and the two `format_human_*` tests structurally
  unchanged.
- **Facade collision with the doctor UPS family.** Mitigation: the
  `ups_` prefix on every newly-exported helper keeps the families
  distinct at the facade level.
- **Snapshot drift from accidental body changes.** None of the four
  `snapshot_human_*` insta snapshots or two `snapshot_json_*` insta
  snapshots will change because the fixture-backed paths and the
  `JsonReport` literals stay byte-identical. Mitigation:
  `cargo test --manifest-path cli/Cargo.toml --lib ups::tests` at each
  sub-commit catches any unexpected `.snap` diff.
- **Forgetting the `mount_point` field in `ups_write_config`.** The
  local body writes a `mount_point` AND a `ups` block; `config_read`
  requires both. Mitigation: the promoted helper's body is the
  byte-identical local body. The two `cmd_ups_status_*` tests would
  fail loudly (config read error) if the field were dropped, so the
  sub-commit 2 verification gate catches this.
- **Overprescribing the test structure.** Implementation may choose to
  leave the four migrated runner-integration tests as-is (only
  `with_output(...)` arguments change to call the new helpers) or to
  consolidate them. The plan requires preserving behavior and the
  strict missing-mock and trailing-newline contracts, not a specific
  assertion layout.

## Verification

Use filtered Rust tests during each sub-commit:

```sh
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib ups::tests
```

Run the full Rust gate at every sub-commit boundary:

```sh
just test-rust
```

Run `cargo check --manifest-path cli/Cargo.toml --tests` as part of
every sub-commit boundary -- not only after adding the fixture module
(sub-commit 1) but also after the migration sub-commit (2), because
sub-commit 2 deletes the local in-place and must leave the module free
of unused imports and dead references.

Behavior-pin tests to run by name at sub-commit 2:

```sh
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::query_ups_returns_query_failed_on_non_zero_exit
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::query_ups_returns_ok_on_healthy_output
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::query_ups_returns_invocation_failed_on_missing_mock
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::cmd_ups_status_invocation_failure_surfaces_typed_error
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::cmd_ups_status_non_zero_exit_is_query_failed
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_human_online
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_human_onbattery
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_human_lowbattery
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_human_replace_battery
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_json_query_failed
cargo test --manifest-path cli/Cargo.toml --lib ups::tests::snapshot_json_not_enabled
```

The four `json_*_fixture_has_expected_shape` tests, the pure-formatter
tests, and the pure-JSON tests are covered by the full `ups::tests`
run; their bodies are untouched.

No VM fixture capture is required. This migration does not change
`cli/tests/fixtures/nixos-25.11/upsc/*.txt`, parser code, nixpkgs
inputs, or production parser behavior.

## Critical Files

- `cli/src/ups.rs` -- the only command-tests module being migrated.
- `cli/src/test_fixtures.rs` -- add `mod ups;`, four facade re-exports, doc bullet.
- `cli/src/test_fixtures/ups.rs` -- new flat ups module (created in sub-commit 1).
- `cli/src/test_fixtures/scrub.rs` -- template for the flat-module shape.
- `cli/src/test_fixtures/shared.rs` -- source of `mock_ok` used internally by the healthy helper.
- `cli/src/test_fixtures/doctor.rs` -- existing UPS-adjacent helpers (`config_with_ups_*`, `UpscSpawnFailureRunner`) intentionally not reused; awareness keeps the `ups_` prefix decision honest.
- `cli/src/cmd.rs` -- `CmdRequest::UpscQuery { name }` variant the helpers compose against.
