# Doctor test fixtures migration

## Context

`cli/src/doctor.rs` contains 69 tests in a 2,114-line `mod tests` block (lines 958-3071) that locally re-implement the same scaffolding pattern the other commands have already migrated away from: config-writing helpers, `DoctorContext` builders, command-output factories, JSON DF constants, and three one-off `CommandRunner` impls. Following the migration of `replace`, `add`, `remove`, `remove_missing`, and `recover` to scope modules under `cli/src/test_fixtures/`, doctor is the last unmigrated test surface in the CLI.

Goal: introduce `cli/src/test_fixtures/doctor.rs`, move only what is genuinely reused or carries non-obvious invariants, leave sharp local fakes in place, and reduce boilerplate without hiding any of the diagnostic contracts each check is pinning. Doctor does not look like the other commands -- it is check-oriented (read state, render result) rather than mutating-command oriented (build params, install topology, run, observe state changes) -- so its fixture module will not follow the `*Pool` + `*ParamsBuilder` + `PoolFixture::*` triad. It will instead be a flat collection of doctor-shaped helpers.

## Current scaffolding by behavior family

Inventory of what is currently inside `mod tests`. Numbers in parentheses are line numbers in `cli/src/doctor.rs`.

### Cross-cutting helpers (used by 4+ families)

- `isolated_paths() -> (TempDir, StatePaths)` (968) -- bare `StatePaths::custom` over a temp dir; no membership, no passphrase. Used ~20x.
- `valid_config_json() -> &'static str` (974) -- canonical doctor config (mount_point + one disk). Used ~20x.
- `mock() -> MockRunner` (978) -- 1-line alias for `MockRunner::default()`. Used ~40x.
- `human_options() -> DoctorOptions` (982) -- `{ json: false, beep: false }`. Used ~30x.
- `write_temp(content: &str) -> NamedTempFile` (997) -- write + flush a temp file. Used ~30x for both config files and notifier configs.
- `find_check<'a>(report, name) -> &'a CheckResult` (1004) -- locate a check by name; panics if absent. Used ~40x.

### Config / schema / permissions (7 tests, 1012-1389)

Pure orchestrator-level tests calling `run_doctor`. Use the cross-cutting helpers above plus inline JSON for malformed/extra-field/empty-mount cases. One test (`dotted_default_path_skips_permissions_lexically`, 1057) calls `check_config_permissions` against a `DoctorContext` built by `parsed_doctor_ctx` (2158).

### Declared disks (17 tests, 1391-1654)

Two sub-shapes:

1. Three prerequisite-gating tests (1391-1428) running `run_doctor` with cascade scenarios.
2. Six pure-summarizer tests targeting `summarize_declared_disks` directly via `cls(name, by_id, state) -> (String, String, DiskState)` (1429). No runner involvement. The damaged/unreadable variants (1453, 1499, 1545) carry **negative** message invariants pinning a cross-command product rule (must NOT mention `/var/lib/braid/luks-headers/` or `.luksheader`).
3. One human-format coverage test (1645).

### Btrfs profile checks (13 tests, 1656-2070)

Per-test scaffolding: `mountpoint_ok` / `mountpoint_fail` (1658, 1672), `df_json(json)` / `df_json_fail()` (1686, 1700), constants `DF_RAID1_CLEAN` (1740) / `DF_MIXED` (1749) / `DF_MIXED_METADATA` (1959). Two cache-validation tests (1873, 1928) lean on `DfQueryFailureRunner` (1714) -- a hand-rolled `CommandRunner` impl that returns `mountpoint_ok` for mountpoint and `Err(CmdError::Failed)` for `BtrfsFilesystemDfJson`. Same-runner cache reuse is the load-bearing invariant: `ensure_df_snapshot` must cache success and error so the second profile check labels the same df failure with its own type ("data" vs "metadata").

### Missing-device checks (5 tests, 2072-2304)

Adds `device_usage_healthy` / `device_usage_with_missing` (2074, 2096), and `PoolMissingDevicesRunner` (2124) -- a hand-rolled `CommandRunner` impl that **panics** on `BtrfsFilesystemDfJson` to enforce that `check_pool_missing_devices` is decoupled from df. Also tracks every call in a `Mutex<Vec<CmdRequest>>`. Plus `parsed_doctor_ctx` (2158) for the panic-runner test.

### Beep probe checks (9 tests, 2306-2578)

All target `check_beep_path_inner` directly with a `beep_ctx` (2316) builder that returns a `DoctorContext` with no config. `beep_check_options(is_root, json_output, play_beep)` (989) packages the triple gate. Three tests (2418, 2455, 2487) implicitly enforce "runner must NOT be invoked" by relying on `MockRunner` returning `MissingMock` on unexpected calls. Two tests (2455, 2487) assert exact message strings (product copy).

### UPS / NUT checks (5 tests, 2583-2773)

Adds `ups_ctx(runner, paths, config_json)` (2584), three config constants (`config_with_ups_enabled` / `_without_ups` / `_disabled`, 2601), `systemctl_is_active_output(state)` (2613), and `UpscSpawnFailureRunner` (2629) -- a hand-rolled `CommandRunner` that returns `Err(CmdError::Failed)` on `UpscQuery`. Spawn failure (`CmdError::Failed`) and nonzero exit (exit=1) drive distinct user-facing messages: spawn failure must NOT suggest `systemctl status upsd`, query failure must.

### Braid-online / systemd safety (9 tests, 2775-3070)

All call `check_braid_online_active_when_mounted` directly with `ups_ctx` + `systemctl_is_active_output`. The hard one is `braid_online_check_reprobes_when_cache_is_stale` (2823) which sets `ctx.mountpoint_is_mounted = Some(false)` *before* calling the check, then asserts the check re-probed (Fail not Skip) -- pinning the safety invariant that the mount cache is not trusted across calls.

## What goes where

### New scope module: `cli/src/test_fixtures/doctor.rs`

The doctor fixture module is a flat collection of helpers, not a `*Pool` topology. Every item is `pub(crate)`.

**Path / file primitives**
- `isolated_paths() -> (TempDir, StatePaths)` -- verbatim port. Doctor-shaped: no membership, no passphrase.
- `write_temp(content: &str) -> NamedTempFile` -- verbatim port. Used for both config files and notifier-config files.

**Default option builders**
- `human_options() -> DoctorOptions` -- canonical non-JSON, non-beep options. `DoctorOptions` is already `pub` with `pub` fields, so this is a verbatim port.
- `beep_check_options(is_root: bool, json_output: bool, play_beep: bool) -> BeepCheckOptions` -- triple-gate. Body: `BeepCheckOptions::for_test(is_root, json_output, play_beep)` (see Visibility section below).

**Config JSON constants**
- `valid_config_json() -> &'static str` -- the canonical mount_point + one-disk config.
- `config_with_ups_enabled() -> &'static str`
- `config_without_ups() -> &'static str`
- `config_with_ups_disabled() -> &'static str`

**`DoctorContext` builders (thin wrappers around doctor.rs constructors -- see Visibility section below)**
- `parsed_doctor_ctx<'a, R: CommandRunner>(runner: &'a R, paths: &'a StatePaths) -> DoctorContext<'a, R>` -- valid config parsed from the canonical doctor JSON, no caches populated. Body: `DoctorContext::for_test_parsed(runner, paths, valid_config_json())` -- single source of truth for the canonical JSON literal lives in the fixture module's `valid_config_json()`, so direct-check tests and `run_doctor`-orchestrated tests can never drift from each other.
- `beep_ctx<'a, R: CommandRunner>(runner: &'a R, paths: &'a StatePaths) -> DoctorContext<'a, R>` -- no config, no caches. Body: `DoctorContext::for_test_beep(runner, paths)`.
- `ups_ctx<'a, R: CommandRunner>(runner: &'a R, paths: &'a StatePaths, config_json: &str) -> DoctorContext<'a, R>` -- config parsed from caller-provided JSON. Body: `DoctorContext::for_test_ups(runner, paths, config_json)`.

**Mock command-output factories**

Each returns `(CmdRequest, RawCommandOutput)` so callers can chain `.with_output(req, out)`:

- `mountpoint_ok()` / `mountpoint_fail()` -- `CmdRequest::MountpointCheck` for `/mnt/storage`.
- `df_json(json: &str)` / `df_json_fail()` -- `CmdRequest::BtrfsFilesystemDfJson` for `/mnt/storage`.
- `device_usage_healthy()` / `device_usage_with_missing()` -- `CmdRequest::BtrfsDeviceUsageRaw` for `/mnt/storage`.

Plus one bare-output factory used in handlers:
- `systemctl_is_active_output(state: &str) -> RawCommandOutput` -- exit code 0 for `active`/`reloading`/`refreshing`, else 3. Returns the raw output, not a paired tuple, because braid-online tests build the request inline (the unit name varies in some skip tests).

**DF JSON corpora**
- `DF_RAID1_CLEAN` -- baseline healthy RAID1 across Data/System/Metadata + GlobalReserve(single).
- `DF_MIXED` -- RAID1 + single in Data block groups.
- `DF_MIXED_METADATA` -- RAID1 + single in Metadata block groups.

**Custom runners (kept as named structs, not closure handlers)**

These three structs each encode a sharp negative invariant. Naming them keeps the panic / spawn-failure intent legible in stack traces and at the use site:

- `DfQueryFailureRunner` -- mountpoint Ok, `BtrfsFilesystemDfJson` returns `Err(CmdError::Failed("df query failed"))`. Drives the "both profile checks warn off the same cached error" assertion.
- `PoolMissingDevicesRunner` -- mountpoint Ok, `BtrfsDeviceUsageRaw` returns `device_usage_healthy`, `BtrfsFilesystemDfJson` **panics**. Tracks every call in a `Mutex<Vec<CmdRequest>>` exposed via a `calls()` accessor.
- `UpscSpawnFailureRunner` -- `UpscQuery` returns `Err(CmdError::Failed(...))`, everything else `Err(CmdError::MissingMock)`. Distinguishes spawn-failure messaging from query-failure messaging.

**Declared-disks summarizer helper**
- `cls(name: &str, by_id: &str, state: DiskState) -> (String, String, DiskState)` -- one-line tuple builder used by the six pure-summarizer tests.

### Visibility model: type bumps and `#[cfg(test)]` constructors

Several types in `cli/src/doctor.rs` are currently module-private. Bumping just the type names to `pub(crate)` is not enough -- `DoctorContext` and `BeepCheckOptions` have all-private fields, and `DoctorContext::df_snapshot: Option<DfSnapshot>` references the module-private `DfSnapshot` (line 92). A sibling-module fixture cannot field-literal-construct these from outside `cli/src/doctor.rs`. The fix:

**Type-level visibility (commit 1):**

- `DiskState` (line 239) -- bump to `pub(crate)`. `cls`'s parameter and return type name it directly. No fields to gate; the enum is constructed via variant names which become crate-visible alongside the enum.
- `DoctorContext` (line 98) -- bump struct itself to `pub(crate)`. Fields stay module-private. Field types (`Config`, `serde_json::Value`, `Option<DfSnapshot>`) need not be exposed because no caller from outside doctor.rs ever names them.
- `BeepCheckOptions` (line 115) -- bump struct itself to `pub(crate)`. Fields stay module-private.
- `DfSnapshot` (line 92) -- stays module-private. The constructors (next bullet) initialize `df_snapshot: None` internally; no caller outside doctor.rs ever names `DfSnapshot`.

**Doc comments on the bumped types (commit 1):** all three types becoming `pub(crate)` are production items, not `#[cfg(test)]` fixtures, so [AGENTS.md's doc-comment rule](../../AGENTS.md#doc-comments) applies. Each gets a one- to three-line `///` comment capturing intent / invariant / ownership at the new boundary, not signature restatement. Sketches:

- `DoctorContext` -- "Per-run state for `braid doctor`: caches mount-probe and df-snapshot results across checks so the orchestrator avoids re-querying btrfs/mountpoint, and threads the parsed config plus `&CommandRunner` borrow that every check needs."
- `BeepCheckOptions` -- "Triple-gate inputs (`is_root`, `json_output`, `play_beep`) for `check_beep_path_inner`. Bundled into a struct so test code can vary one axis at a time without growing the call signature."
- `DiskState` -- "Classification of a single declared disk after the doctor's LUKS probe. `summarize_declared_disks` translates a slice of these into a `CheckResult`; the variants pin the four reachable outcomes (header Ok, header unreadable, header damaged, missing/non-block/probe-failed)."

The `#[cfg(test)] pub(crate)` constructors below are exempt from the doc-comment rule (AGENTS.md "Skip: `#[cfg(test)]` items and test fixtures"), as are the items inside `cli/src/test_fixtures/doctor.rs` itself.

**`#[cfg(test)] pub(crate)` constructors in `cli/src/doctor.rs` (commit 1):**

These are added at module scope (not inside `mod tests`) so they are visible to crate-wide test code. They handle all field-literal construction inside doctor.rs where private fields are in scope.

```rust
#[cfg(test)]
impl<'a, R: CommandRunner> DoctorContext<'a, R> {
    pub(crate) fn for_test_parsed(
        runner: &'a R,
        paths: &'a StatePaths,
        config_json: &str,
    ) -> Self {
        let value: serde_json::Value =
            serde_json::from_str(config_json).expect("test config JSON parses");
        let config: Config =
            serde_json::from_value(value.clone()).expect("test config parses");
        Self {
            config_path: PathBuf::new(),
            config_value: Some(value),
            config: Some(config),
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }

    pub(crate) fn for_test_beep(runner: &'a R, paths: &'a StatePaths) -> Self {
        Self {
            config_path: PathBuf::new(),
            config_value: None,
            config: None,
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }

    pub(crate) fn for_test_ups(
        runner: &'a R,
        paths: &'a StatePaths,
        config_json: &str,
    ) -> Self {
        let value: serde_json::Value =
            serde_json::from_str(config_json).expect("test config parses");
        let config: Option<Config> = serde_json::from_str(config_json).ok();
        Self {
            config_path: PathBuf::new(),
            config_value: Some(value),
            config,
            runner,
            paths,
            mountpoint_is_mounted: None,
            df_snapshot: None,
        }
    }
}

#[cfg(test)]
impl BeepCheckOptions {
    pub(crate) fn for_test(is_root: bool, json_output: bool, play_beep: bool) -> Self {
        Self { is_root, json_output, play_beep }
    }
}
```

**Why constructors and not `pub(crate)` fields:** field-level `pub(crate)` exposes mutation across the crate, including non-test code. `#[cfg(test)] pub(crate)` constructors keep production-side internals encapsulated while letting test code build the same shapes. Tests inside `cli/src/doctor.rs`'s `mod tests` retain direct field-mutation access (e.g. `ctx.mountpoint_is_mounted = Some(false)` at the stale-cache reprobe test, `ctx.config_path = PathBuf::from("...")` at the dotted-path test) because they live in the same module as the struct definitions.

**Public surface of `cli/src/doctor.rs`:** `DoctorOptions` (already `pub`), `run_doctor`, `CheckStatus`, `CheckResult`, `DoctorReport` are all `pub`, so the migrated `#[test]` functions retain access. Internal helpers like `summarize_declared_disks`, `check_beep_path_inner`, and `check_braid_online_active_when_mounted` stay module-private; the migrated tests still live in `cli/src/doctor.rs`'s `mod tests` and reach them by their bare names.

### Wiring

In `cli/src/test_fixtures.rs`, register the new module and re-export its public surface:

```rust
mod doctor;
// ...
#[allow(unused_imports)]
pub(crate) use doctor::{
    DfQueryFailureRunner, PoolMissingDevicesRunner, UpscSpawnFailureRunner,
    DF_MIXED, DF_MIXED_METADATA, DF_RAID1_CLEAN,
    beep_check_options, beep_ctx, cls, config_with_ups_disabled, config_with_ups_enabled,
    config_without_ups, device_usage_healthy, device_usage_with_missing, df_json, df_json_fail,
    human_options, isolated_paths, mountpoint_fail, mountpoint_ok, parsed_doctor_ctx,
    systemctl_is_active_output, ups_ctx, valid_config_json, write_temp,
};
```

### Migration mechanics: fully-qualified paths during sub-commits

Every fixture-module item shadows a same-named item still defined inside `cli/src/doctor.rs`'s `mod tests`. A module-scope `use crate::test_fixtures::mountpoint_ok;` introduced in commit 2 would collide with the still-present local `fn mountpoint_ok()` (1658) under E0252 ("the name `mountpoint_ok` is defined multiple times"). The same is true for `df_json`, `isolated_paths`, `write_temp`, `DfQueryFailureRunner`, `PoolMissingDevicesRunner`, `UpscSpawnFailureRunner`, and every other migrated helper that has a same-named local. Because the migration is incremental, the local definitions cannot be deleted before all of their call sites have moved.

The sub-commit strategy resolves this by deferring the broad `use` block to the cleanup commit:

- **Commits 2-11 (per-test migration):** migrated test bodies call fixture helpers via fully-qualified `crate::test_fixtures::foo()` paths inline. No `use crate::test_fixtures::...` lines are added at module scope. Local helper definitions remain untouched. The same goes for fixture-only types referenced by name -- e.g. constructing the panic runner becomes `let runner = crate::test_fixtures::PoolMissingDevicesRunner::default();`. Constants are referenced as `crate::test_fixtures::DF_RAID1_CLEAN`. Tests that need the fixture-side `DoctorContext` constructor wrappers call `crate::test_fixtures::parsed_doctor_ctx(&runner, &paths)`.

- **Commit 12 (cleanup):** all locally-defined doctor helpers listed in the cleanup row are deleted. In the same commit, a single `use crate::test_fixtures::{...};` block (the one shown above) is inserted at the top of `mod tests`, and all the inline `crate::test_fixtures::foo` paths in commits 2-11 are un-qualified to bare names that resolve through the new `use`. This is a mechanical pass; `cargo check --tests` confirms greenness.

**Why not per-test `use` statements during migration:** function-scope `use` does shadow module-scope items in Rust (so `fn migrated_test() { use crate::test_fixtures::mountpoint_ok; ... }` would compile cleanly even with the local `fn mountpoint_ok()` still defined), but it scatters the import surface across 60+ test bodies and adds a second cleanup pass to consolidate. Fully-qualified inline paths keep all import bookkeeping for commit 12.

**End state (after commit 12):** doctor's `mod tests` opens with the consolidated `use crate::test_fixtures::{...};` block and contains no local helper definitions besides `find_check`. This matches the shape of the existing migrated test files (`replace.rs` 2549, `recover.rs` 3275, `remove_missing.rs` 585).

### Stays test-local in `cli/src/doctor.rs`'s `mod tests`

- `mock()` -- 1-line alias for `MockRunner::default()`. Drop it; tests use `MockRunner::default()` directly. Removing it is a tiny inline expansion; not worth a fixture-module export.
- `find_check(report, name)` -- pure assertion-side query helper, not scaffolding. Keeps the `mod tests` block self-explanatory for readers tracing assertions.
- Single-use inline JSON in cascade tests (e.g. `r#"{"disks":{},"mount_point":"/mnt/storage"}"#` at 1416, the malformed `"not json at all {{{"` at 1095, etc.) -- these encode the specific defect each cascade test exercises and are clearer at the use site.

### `cli/src/test_fixtures/shared.rs` -- no changes for this migration

`shared::mock_ok` is a `RawCommandOutput`-only constructor. Doctor's factories return `(CmdRequest, RawCommandOutput)` tuples sized for `.with_output` chaining, which is a different ergonomic shape; refactoring doctor's factories to use `mock_ok` internally would not change the public surface and is not required.

`shared::MockFs`, `shared::PoolFixture`, and `shared::TEST_PASSPHRASE_BYTES` model pool topologies (mountinfo, membership, passphrase). Doctor reads config and runs checks; it does not own a pool. None of these belong in doctor's fixtures, and none of doctor's helpers belong in shared at this time. If a later change makes doctor exercise a pool-topology helper, that promotion can happen in a follow-up.

## Hard cases to migrate first

These are the validation set: if the fixture design works for these, the bulk migration is mechanical. Each is migrated as a single sub-commit so any breakage is bisectable. Single-test filters live in the verification section.

1. **`pool_missing_devices_does_not_require_filesystem_df`** (2202). Migrates `PoolMissingDevicesRunner` (panic-on-df + `Mutex<Vec<CmdRequest>>` call log) and `parsed_doctor_ctx`. The panic must survive the move verbatim. After migration, the test should still construct the runner with `PoolMissingDevicesRunner::default()` and pull calls via the same accessor.

2. **`braid_online_check_reprobes_when_cache_is_stale`** (2823). Migrates `ups_ctx`, `systemctl_is_active_output`, and `config_with_ups_enabled`. Validates that fixture-built contexts can still be mutated post-construction (the test does `ctx.mountpoint_is_mounted = Some(false)` before calling the check). Confirms the fixture does not over-encapsulate -- `DoctorContext` fields must remain field-accessible from `mod tests`.

3. **`profile_checks_warn_when_df_query_errors`** (1873) **and** **`profile_checks_warn_when_df_json_malformed`** (1928). Migrates `DfQueryFailureRunner`, `mountpoint_ok`, `df_json`. Validates that the same `MockRunner` (or hand-rolled fixture runner) can still feed two profile checks that share `df_snapshot`, and that the fixture-imported helpers and the live `run_doctor` orchestration co-exist.

4. **`summarize_warn_luks_header_unreadable`** (1452) **and** **`summarize_warn_luks_header_damaged`** (1499). Migrates `cls`. Pure tests, no runner. The negative message invariants (must NOT contain `/var/lib/braid/luks-headers/` or `.luksheader`) must remain assertable verbatim; if the fixture move accidentally changes how `DiskState` is constructed (e.g. by wrapping a builder), these assertions stay the canary.

## Sub-commit plan

Each commit is independently green: `cargo check --tests`, `cargo test --lib doctor::tests`, and `just test-rust` all pass at every boundary. Greenness is preserved by the fully-qualified-path strategy described in the Migration mechanics section: migration commits 2-11 do not introduce module-scope `use crate::test_fixtures::...` imports, so they cannot collide with same-named locals that linger in `mod tests` until cleanup. Under Conventional Commits with the `test(doctor)` and `refactor(test-fixtures)` types:

| # | Commit subject | Scope | Notes |
|---|---|---|---|
| 1 | `refactor(test-fixtures): scaffold doctor scope module` | In `cli/src/doctor.rs`: write the `///` boundary doc comments on `DiskState`, `DoctorContext`, `BeepCheckOptions` per the Visibility section (AGENTS.md doc-comment rule applies because these become production `pub(crate)` items). Bump those three types to `pub(crate)` (struct/enum level only -- fields stay private). Add the `#[cfg(test)] pub(crate)` constructors `DoctorContext::for_test_parsed(runner, paths, config_json: &str)`, `DoctorContext::for_test_beep`, `DoctorContext::for_test_ups`, `BeepCheckOptions::for_test` at module scope (not inside `mod tests`); the constructors are `#[cfg(test)]`-gated and exempt from the doc-comment rule. Add `cli/src/test_fixtures/doctor.rs`, register it in `test_fixtures.rs`, populate it with the helpers from the API surface above (the `*_ctx` and `beep_check_options` fixture wrappers call the new constructors; `parsed_doctor_ctx`'s body is `DoctorContext::for_test_parsed(runner, paths, valid_config_json())`). Update the `test_fixtures.rs` doc-comment to mention the new module. No test changes yet. | Compiles green; no doctor tests yet use it. `cargo check --tests` is the proof. |
| 2 | `test(doctor): migrate pool_missing_devices custom runner` | Migrate `pool_missing_devices_does_not_require_filesystem_df` (2202) to fully-qualified `crate::test_fixtures::PoolMissingDevicesRunner`, `crate::test_fixtures::parsed_doctor_ctx`, `crate::test_fixtures::device_usage_healthy`, `crate::test_fixtures::mountpoint_ok` paths inline. Do NOT delete any local helper definitions; do NOT add a module-scope `use crate::test_fixtures::...` line. | Validates the panic-runner port. |
| 3 | `test(doctor): migrate braid_online stale-cache reprobe` | Migrate `braid_online_check_reprobes_when_cache_is_stale` to fixture `ups_ctx` + `systemctl_is_active_output`. Confirms post-construction mutation still works. | Validates context mutability. |
| 4 | `test(doctor): migrate profile_checks df-cache pair` | Migrate `profile_checks_warn_when_df_query_errors` and `profile_checks_warn_when_df_json_malformed` to the fixture `DfQueryFailureRunner` and paired (mountpoint, df) helpers. | Validates cache reuse across two checks. |
| 5 | `test(doctor): migrate declared-disks pure summarizer tests` | Migrate the six `summarize_*` tests to the fixture `cls` helper. Pure tests; no runner involved. Leaves the prerequisite-gating tests (1391, 1400, 1413) and the human-format coverage test (1645) for later commits. | Validates `cls` export. |
| 6 | `test(doctor): migrate config / schema / permissions family` | In tests at 1012-1389: rewrite local `isolated_paths`, `valid_config_json`, `human_options`, `write_temp` calls to fully-qualified `crate::test_fixtures::*` paths. Drop the `mock()` alias at each call site -- use `MockRunner::default()` inline. Local helper definitions stay in place (deletion is in commit 12). | Bulk family 1. |
| 7 | `test(doctor): migrate declared-disks gating + human-format tests` | Migrate the three prerequisite-gating tests (1391, 1400, 1413) and the human-format test (1645). | Bulk family 2. |
| 8 | `test(doctor): migrate btrfs profile family` | Migrate the remaining 11 profile tests (1759-2070) to fixture `mountpoint_ok/fail`, `df_json/df_json_fail`, `DF_RAID1_CLEAN`, `DF_MIXED`, `DF_MIXED_METADATA`. | Bulk family 3. |
| 9 | `test(doctor): migrate missing-devices family` | Migrate the remaining 4 missing-device tests (2179, 2237, 2275, 2288). | Bulk family 4. |
| 10 | `test(doctor): migrate beep probe family` | Migrate all 9 beep tests (2338-2575) to fixture `beep_ctx` + `beep_check_options` + `write_temp`. | Bulk family 5. |
| 11 | `test(doctor): migrate UPS daemon and braid-online families` | Migrate the remaining 4 UPS-daemon tests (2656, 2680, 2719, 2743, 2761) and the remaining 8 braid-online tests (2784, 2858, 2891, 2928, 2968, 3014, 3039, 3058). | Bulk families 6+7. |
| 12 | `refactor(test-fixtures): drop migrated doctor.rs scaffolding` | Delete the now-unused local helpers from `cli/src/doctor.rs`'s `mod tests`: `isolated_paths`, `valid_config_json`, `mock` (entirely -- migrated tests use `MockRunner::default()` directly), `human_options`, `beep_check_options`, `write_temp`, `mountpoint_ok/fail`, `df_json/df_json_fail`, `device_usage_healthy/with_missing`, `DfQueryFailureRunner`, `PoolMissingDevicesRunner`, `UpscSpawnFailureRunner`, `parsed_doctor_ctx`, `beep_ctx`, `ups_ctx`, `cls`, `systemctl_is_active_output`, `config_with_ups_*`, `DF_RAID1_CLEAN`, `DF_MIXED`, `DF_MIXED_METADATA`. Keep `find_check`. In the same commit: add the consolidated `use crate::test_fixtures::{...};` block at the top of `mod tests` (per the Wiring section), and replace every fully-qualified `crate::test_fixtures::foo` path introduced in commits 2-11 with its bare name `foo`. The cleanup is mechanical -- a single search-and-replace pass per identifier. | Cleanup; the file should drop from ~3,071 lines to roughly 2,700. `cargo check --tests` confirms greenness. |
| 13 | `docs(plans): promote doctor test fixtures plan` | `git mv plans/wip/draft-a-migration-plan-playful-iverson.md plans/impl/<date>-doctor-test-fixtures-migration.md`. | Plan promotion. |

Some commits within the bulk-migration phase (6-11) may be split if the diffs are large; a per-family split keeps each commit reviewable. The order of bulk families is irrelevant to greenness because (a) commit 1 populates the entire fixture module up front, and (b) every fixture call in commits 2-11 is a fully-qualified `crate::test_fixtures::foo` path -- those paths are valid the moment commit 1 lands, regardless of which other tests have or have not been migrated.

## Verification

At every sub-commit boundary:

- `cargo check --manifest-path cli/Cargo.toml --tests`
- `cargo test --manifest-path cli/Cargo.toml --lib doctor::tests`
- `just test-rust` (covers the broader unit-test suite, including the parser-fixture goldens, so that incidental test-fixture-module changes don't regress non-doctor tests)

Targeted single-test filters for hard-case migrations:

```bash
cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::pool_missing_devices_does_not_require_filesystem_df

cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::braid_online_check_reprobes_when_cache_is_stale

cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::profile_checks_warn_when_df_query_errors

cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::profile_checks_warn_when_df_json_malformed

cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::summarize_warn_luks_header_unreadable

cargo test --manifest-path cli/Cargo.toml --lib \
    doctor::tests::summarize_warn_luks_header_damaged
```

Smoke run before the cleanup commit (#12): `cargo test --manifest-path cli/Cargo.toml --lib doctor` (matches all 69 tests in the family).

The full test loop after promotion: `just test-rust && just test-rust-unstable` so unstable golden coverage stays green.

## Risks

**Fixture rigidity for cascade tests.** The config / schema / permissions family asserts cascade behavior (a Fail at config_file forces Skip at config_schema and config_permissions). If a fixture helper subtly changes the config_path (e.g. wraps `write_temp` in a way that produces a different parent directory) the canonical-permissions check might flip from Skip to Ok or vice versa. **Mitigation:** keep `write_temp` API verbatim (`fn(content: &str) -> NamedTempFile`) and assert canonical-permission Skip in the smoke pass before commit 12.

**Hidden command calls via `parsed_doctor_ctx`.** `parsed_doctor_ctx` builds a `DoctorContext` with `config_path: PathBuf::new()`. Any check that reads `ctx.config_path` to derive a probe target would observe an empty path. Today only `check_config_permissions` reads it for the canonical-path equality test, but if a future check ever started using it, the fixture-built context would silently differ from a `run_doctor`-built one. **Mitigation:** the doc comment on `parsed_doctor_ctx` in the fixture module must spell out that `config_path` is empty by design and that the helper is for tests calling individual `check_*` functions directly, not `run_doctor`.

**Cached state across checks.** `ensure_df_snapshot` / `ensure_mountpoint_is_mounted` cache results inside a `DoctorContext`. The fixture `*_ctx` builders return contexts with `mountpoint_is_mounted: None` and `df_snapshot: None`, but a test can still mutate fields post-construction (as `braid_online_check_reprobes_when_cache_is_stale` does). **Mitigation:** keep the `DoctorContext` fields publicly accessible from `mod tests`; do not introduce setter methods or builders that would force the test to migrate to a different mutation API. The cache-validation tests (1873, 1928) depend on `run_doctor`'s sequencing: they construct one runner, run all checks, and assert both profile checks warn off the same cached error -- this only works if `run_doctor` builds a single context internally. The fixture helpers do not change this behavior; they only build per-test-direct-call contexts.

**Config-path semantics for permissions.** The `dotted_default_path_skips_permissions_lexically` test (1057) verifies that `/etc/braid/./config.json` is treated as a *custom* path (Skip, not the canonical permissions enforcement). The fixture-built `parsed_doctor_ctx` populates `config_path: PathBuf::new()`; the test then re-assigns `ctx.config_path = PathBuf::from("/etc/braid/./config.json")`. **Mitigation:** preserve the post-construction mutation pattern; do not introduce a `parsed_doctor_ctx_at(path: &Path)` constructor that would tempt the test to pre-populate config_path and obscure the intent.

**Exact diagnostic message strings.** Two beep tests (2455, 2487) and one braid-online test (2928) use `assert_eq!` on the full message string. `cli/src/test_fixtures/doctor.rs` does not own any of these messages -- they live in `doctor.rs` itself -- but it does own the contexts and runners that drive the checks. **Mitigation:** the migration touches none of doctor's check-side code; an accidental whitespace change in a fixture string constant cannot affect product copy. The cargo test grid catches any regression here.

**`with_handler` over-application temptation.** It is tempting to convert `DfQueryFailureRunner`, `PoolMissingDevicesRunner`, and `UpscSpawnFailureRunner` to closure-based handlers on `MockRunner`. They are conceptually simple variant matches. But each encodes a sharp diagnostic boundary -- especially the panic-on-df in `PoolMissingDevicesRunner` -- and named structs make those boundaries legible at the use site (`let runner = PoolMissingDevicesRunner::default();` self-documents the test's intent in a way `MockRunner::default().with_handler(|req| match req { ... panic ... })` does not). **Mitigation:** keep them as named structs in the fixture module. Doctor's tests do not have topology-shaped or repeated-state-flip patterns that would benefit from `with_handler`; the existing direct `with_output` chains are clearer.

**Beep tests' implicit "runner must not be invoked" invariant.** Three beep tests (2418, 2455, 2487) rely on `MockRunner::default()` returning `CmdError::MissingMock` if the runner is unexpectedly invoked. After migration, the same `MockRunner::default()` call from inside the test is preserved; the fixture `beep_ctx` does not configure a runner. **Mitigation:** the migrated test must continue to construct its own `MockRunner::default()` (not pull one from the fixture). The fixture exports `beep_ctx(runner, paths)` which takes a runner reference -- this is intentional and matches the existing API.

**`write_temp` lifetime ambiguity.** The current `write_temp` returns a `NamedTempFile` that the caller binds to a local; the test then calls `.path()` on it to pass into `run_doctor`. The fixture port preserves the same return type. **Mitigation:** not a structural risk, but reviewers should sanity-check that the migrated tests still bind `write_temp` to a named local before borrowing `.path()`, since dropping the `NamedTempFile` deletes the file.

## Critical files

- `cli/src/doctor.rs` -- 69 test functions in `mod tests` (958-3071). All migrations land here.
- `cli/src/test_fixtures.rs` -- umbrella re-exports; add `mod doctor;` and the public re-export block.
- `cli/src/test_fixtures/doctor.rs` -- new file. ~250-350 lines target size based on the inventory.
- `cli/src/test_fixtures/shared.rs` -- read-only reference for this migration; no edits.
- `cli/src/cmd.rs` -- `MockRunner` / `MockRunner::with_handler` (957-1027); read-only reference.

## Reused fixtures and utilities (no new code)

- `MockRunner::default()`, `MockRunner::with_output(req, out)` (`cli/src/cmd.rs:988`) -- continue to be the primary mock-output API for doctor.
- `MockRunner::with_handler` (`cli/src/cmd.rs:1021`) -- not used by this migration.
- `StatePaths::custom` -- continue to be how `isolated_paths` builds its `StatePaths`.
- `Config`, `DoctorOptions`, `CheckResult` -- already `pub`. The fixture module imports them by their crate paths (`use crate::config::Config;`, `use crate::doctor::{DoctorOptions, CheckResult};` as needed).
- `DoctorContext`, `BeepCheckOptions`, `DiskState` -- become `pub(crate)` in commit 1. Fixture module imports them by crate path; field-literal construction stays inside `cli/src/doctor.rs` via the `#[cfg(test)] pub(crate)` constructors added in commit 1.
- `DfSnapshot` -- stays module-private. Constructors initialize the field to `None`; no caller outside `cli/src/doctor.rs` ever names this type.
