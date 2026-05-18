# Plan: dissolve `for_test_ups` into `for_test_parsed`

## Context

`DoctorContext::for_test_ups` at `cli/src/doctor.rs:1059-1072` silently
swallows `Config` deserialization errors via `.ok()`:

```rust
let value: serde_json::Value =
    serde_json::from_str(config_json).expect("test config parses");
let config: Option<Config> = serde_json::from_str(config_json).ok();
```

If a contributor mistypes a fixture JSON string, `config` becomes `None`
and every UPS-enabled doctor test silently flips to the
"config unavailable" skip branch -- the test reports green while never
exercising the check it was meant to cover.

A code-review finding flagged this and proposed adding
`for_test_ups_with_unparsed_config(...)` to preserve the silent-drop
semantics for one outlier test. Verification (`/verify-issue`)
established that:

1. **No caller needs the silent drop.** All 15 invocations of
   `ups_ctx` in `cli/src/doctor.rs` pass either
   `config_with_ups_enabled()` or `config_without_ups()` -- both static,
   well-formed JSON and well-formed `Config`.
2. **The "valid JSON / bad schema" path is already covered elsewhere.**
   `valid_json_bad_schema_skips_ups_as_config_unavailable`
   (`cli/src/doctor.rs:1253`) drives the full `run_doctor` entry point
   with a `write_temp` file -- it never touches `for_test_ups`.
3. **`for_test_ups` is byte-equivalent to `for_test_parsed`** once
   `.ok()` is replaced with `.expect(...)`. `Config.ups` is
   `Option<Ups>` (`cli/src/config.rs:42`), so both UPS-enabled and
   UPS-absent fixtures deserialize cleanly through `for_test_parsed`.

The right pivot is therefore structural: delete `for_test_ups` entirely
and redirect `ups_ctx` to `for_test_parsed`. This fixes the silent-
swallow hazard *and* eliminates a redundant constructor in one move --
no new `for_test_ups_with_unparsed_config` needed.

Intended outcome: a fixture typo in any UPS test fails loudly at
construction time with `"test config parses"`, and the test-only
constructor surface shrinks by one function.

## Change

### 1. Delete `for_test_ups`

**File:** `cli/src/doctor.rs`

Remove the entire constructor at lines 1059-1072:

```rust
pub(crate) fn for_test_ups(runner: &'a R, paths: &'a StatePaths, config_json: &str) -> Self {
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
```

No other code in the crate references `for_test_ups` directly --
`ups_ctx` in the fixture module is the sole caller.

### 2. Redirect `ups_ctx` to `for_test_parsed`

**File:** `cli/src/test_fixtures/doctor.rs:98-104`

Change one line in the body:

```rust
pub(crate) fn ups_ctx<'a, R: CommandRunner>(
    runner: &'a R,
    paths: &'a StatePaths,
    config_json: &str,
) -> DoctorContext<'a, R> {
    DoctorContext::for_test_parsed(runner, paths, config_json)  // was: for_test_ups
}
```

`ups_ctx`'s name (and the 15 call sites that use it) stays put.
Renaming is out of scope; the name still reads clearly at the use
sites and a rename would inflate the diff without dissolving any
issue.

### 3. Pin the loud-failure contract with a regression test

**File:** `cli/src/doctor.rs` (inside `#[cfg(test)] mod tests`)

Without an automated test, the silent-drop regression could quietly
return -- a future contributor could reintroduce `.ok()` (on
`for_test_parsed`, `ups_ctx`, or a new variant) and the entire UPS
suite would still go green because every fixture passes valid JSON.
Add a `#[should_panic]` test that drives `ups_ctx` with JSON that
parses as `Value` but fails `Config` schema validation:

```rust
// Intent: ups_ctx panics loudly when config JSON parses as Value but
// fails Config schema validation.
// Why it exists: ups_ctx historically built ctx.config = None on schema
// failure via .ok(), letting mistyped fixtures silently flip UPS tests
// to the "config unavailable" skip branch. This pins the loud-failure
// contract so the regression cannot reappear unnoticed.
// Scenario: a future contributor reintroduces a silent-drop builder
// (whether on ups_ctx, for_test_parsed, or a new variant).
#[test]
#[should_panic(expected = "test config parses")]
fn ups_ctx_panics_on_schema_invalid_config() {
    let runner = MockRunner::default();
    let (_dir, paths) = isolated_paths();
    let _ctx = ups_ctx(
        &runner,
        &paths,
        r#"{"mount_point":"","ups":{"name":"ups"}}"#,
    );
}
```

Why this shape works:

- The JSON parses as `serde_json::Value` (so the outer
  `from_str(...).expect("test config JSON parses")` succeeds), but
  empty `mount_point` trips the `try_from = "RawConfig"` check in
  `cli/src/config.rs:38-43`, producing
  `ConfigBuildError::EmptyMountPoint` ("mount_point must not be
  empty"). The inner `from_value(...).expect("test config parses")`
  then panics with `"test config parses: <err>"`.
- `#[should_panic(expected = ...)]` matches on substring, so the
  `"test config parses"` literal matches whether the panic message is
  the bare literal or the wrapped form with the serde error appended.
- The same JSON shape is already used by
  `valid_json_bad_schema_skips_ups_as_config_unavailable` at
  `cli/src/doctor.rs:1253` to drive the `run_doctor` schema-failure
  branch, so the test reuses an established fixture pattern.

## Reuse

- `DoctorContext::for_test_parsed` at `cli/src/doctor.rs:1032-1045` --
  the existing constructor we delegate to. Identical field defaults
  (empty `config_path`, populated `config_value`/`config`, `None` for
  `mountpoint_is_mounted`/`df_snapshot`) and identical parse-twice-
  into-Value-and-Config shape. Its `.expect("test config parses")` on
  the inner `from_value` call is exactly the loud-failure semantics we
  want.
- `valid_json_bad_schema_skips_ups_as_config_unavailable` at
  `cli/src/doctor.rs:1253` -- the established pattern (write JSON to a
  tempfile, drive `run_doctor`) for future tests that need to exercise
  the schema-failure branch end-to-end.

## Verification

Run from the repo root:

1. **`just test-rust`** -- this single command covers both lanes now
   that the loud-failure check is automated:
   - Happy path: every UPS test (`ups_daemon_check_*`,
     `check_braid_online_active_when_mounted` UPS variants, ~15 sites)
     still passes. The full doctor test module compiles only if the
     `for_test_ups` removal hasn't left a dangling reference.
   - Regression guard: the new
     `ups_ctx_panics_on_schema_invalid_config` test fails loudly if a
     future change reintroduces silent-drop semantics anywhere on the
     `ups_ctx` -> `for_test_parsed` path.

No NixOS VM tests or parser fixtures are involved; this is a test-only
refactor inside the Rust CLI crate.

## Files modified

- `cli/src/doctor.rs` -- delete `for_test_ups` (13-line removal); add
  `ups_ctx_panics_on_schema_invalid_config` regression test inside
  `#[cfg(test)] mod tests` (~15 lines including the preamble).
- `cli/src/test_fixtures/doctor.rs` -- swap `for_test_ups` ->
  `for_test_parsed` (1-line change).

Net: ~13 lines deleted, 1 line changed, ~15 lines added in test code.
No production code touched.
