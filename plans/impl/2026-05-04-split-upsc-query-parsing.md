# Plan: split parse_upsc's runner-integration concern from its parsing concern

## Context

Today `parse_upsc` returns `Result<UpscOutput, ParseError>` (`cli/src/parse/upsc.rs:49`),
where `ParseError` is a shared parser-layer enum with five variants
(`cli/src/parse/mod.rs:43-66`). In practice the function only ever emits
`CommandFailed` (`cli/src/parse/upsc.rs:50-56`), because the body of the parse
is forgiving: unknown keys go to `extra`, malformed percents become `None`,
missing keys leave `Option` fields `None`. The exit-status check at the top is
the function's only failure mode.

Four call sites consume this with a catch-all `Err(_)` arm and emit messages
that point operators at the upsd daemon -- even though `CmdRequest::UpscQuery`
documents non-zero exit as "the upsd daemon is unreachable or the UPS name is
unknown" (`cli/src/cmd.rs:228-231`):

| Site                                            | Behavior                                                                                                  |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `cli/src/ups.rs:76-78`                          | `Err(_) => emit_daemon_down(json)` -- error string at line 22 says "ups daemon not running -- check 'systemctl status upsd.service'" |
| `cli/src/doctor.rs:661-668`                     | `Err(_) => CheckResult::warn(... "check 'systemctl status upsd.service'")`                                |
| `cli/src/preflight.rs:472-475`                  | `Err(_) => refuse("upsc output unparseable or upsd unreachable")` -- already hedged                       |
| `cli/src/tui/probe.rs:593-595`                  | `Err(_) => ups_snapshot_daemon_down(runner)`                                                              |

Two structural problems:

1. **Five-variant catch-all.** A future `parse_upsc` error variant would be
   silently classified as "daemon down" at all four sites.
2. **Misleading naming.** "DaemonDown" is too narrow for non-zero `upsc` exit.
   A wrong UPS name, comms-failure, or other fatal NUT path produces the same
   exit non-zero, but the messaging only points at upsd.

The fix is structural: split parsing (forgiving text walk) from
runner-integration (subprocess spawned + exited zero). Parsing becomes
infallible; the runner+exit-status boundary becomes its own helper with a
closed, neutrally-named error enum the call sites own. Every call site
matches the variants by name (no `Err(_)`) so future variants force a
compile-time re-decision at every site.

Precedent for an infallible parser already exists: `parse_smartctl`
(`cli/src/parse/smartctl.rs:76`) returns `SmartProbe` directly with `Unknown`
sentinels instead of `Result`.

## Approach

Two-layer refactor across seven files (six Rust + one VM canary). Each
layer is self-contained.

### 1. Make `parse_upsc` infallible

`cli/src/parse/upsc.rs:49` changes from:

```rust
pub fn parse_upsc(raw: &RawCommandOutput) -> Result<UpscOutput, ParseError>
```

to:

```rust
pub fn parse_upsc(stdout: &str) -> UpscOutput
```

Drop the `if raw.exit_status != 0 { return Err(...) }` block at lines 50-56.
The body only reads `raw.stdout`; `&str` is sufficient. The terminal `Ok(...)`
becomes a bare `UpscOutput { ... }`.

The module-level doc-comment at `cli/src/parse/upsc.rs:11-14` ("a non-zero
`upsc` exit becomes `ParseError::CommandFailed`...") is replaced with a note
that this parser is infallible by design and that subprocess-failure
classification lives in `query_ups` (see section 2).

### 2. Add `query_ups` + `UpsQueryError` in `cli/src/ups.rs`

Lives next to the existing `UpsError` enum (`cli/src/ups.rs:18-25`). Reuses
`CmdRequest::UpscQuery { name: String }` (`cli/src/cmd.rs:232-234`),
`CommandRunner`, and `CmdError` (`cli/src/cmd.rs:818-823`).

```rust
#[derive(Debug, thiserror::Error)]
pub enum UpsQueryError {
    /// Runner-level failure: spawn error, signal-killed child, IO error
    /// writing stdin, request/mode mismatch. Wraps `CmdError`, which
    /// already encodes all of these.
    #[error("upsc invocation failed: {0}")]
    InvocationFailed(#[from] CmdError),
    /// upsc exited non-zero. Per `cli/src/cmd.rs:228-231`, this means the
    /// upsd daemon is unreachable, the UPS name is unknown, or another
    /// fatal NUT path. The captured `stderr` is the only safe diagnostic
    /// to surface; the call sites render it verbatim.
    #[error("upsc query failed (exit {exit_code}): {stderr}")]
    QueryFailed { exit_code: i32, stderr: String },
}

pub fn query_ups<R: CommandRunner>(
    runner: &R,
    name: &str,
) -> Result<UpscOutput, UpsQueryError> {
    let raw = runner.run(&CmdRequest::UpscQuery { name: name.to_owned() })?;
    if raw.exit_status != 0 {
        return Err(UpsQueryError::QueryFailed {
            exit_code: raw.exit_status,
            stderr: raw.stderr.trim().to_owned(),
        });
    }
    Ok(parse_upsc(&raw.stdout))
}
```

Why `InvocationFailed` (not `SpawnFailed`): `CmdError::Failed`
(`cli/src/cmd.rs:817-823` and call sites at lines 868, 891, 910, 915, 920,
929, 942) covers signal-killed children, stdin write failures, and request-
mode mismatches in addition to spawn errors. "Invocation" is the umbrella
term that covers all of these without falsely narrowing.

Why `QueryFailed` (not `DaemonDown`): non-zero exit from `upsc` covers more
than daemon-unreachable per the `CmdRequest::UpscQuery` doc-comment
(`cli/src/cmd.rs:228-231`). The neutral name lets call sites surface the
captured stderr, so the operator sees upsc's own error (e.g. "Unknown UPS"
vs "Connection failure: Connection refused") and can act on the actual
cause.

The `#[from] CmdError` lets `runner.run(...)?` route automatically.
`UpsQueryError` is internal to the cli crate; no `#[non_exhaustive]` needed.

### 3. Reshape `UpsError` and `JsonReport` in `cli/src/ups.rs`

`UpsError::DaemonDown` (variant + the misleading "ups daemon not running --
check 'systemctl status upsd.service'" message at `cli/src/ups.rs:21-22`) is
replaced with a neutral `QueryFailed` carrying the captured detail:

```rust
#[derive(Debug, thiserror::Error)]
pub enum UpsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("upsc query failed: {detail}")]
    QueryFailed { detail: String },
    #[error("failed to serialize ups status: {0}")]
    Serialize(#[source] serde_json::Error),
}
```

The `JsonReport` enum at `cli/src/ups.rs:30-36` and its sentinel string also
update -- `DaemonDown { error: "daemon_down" }` becomes:

```rust
QueryFailed { error: &'static str, detail: &'a str }
```

with `error: "query_failed"`. The `--json` consumer now sees the captured
upsc stderr in the `detail` field.

`emit_daemon_down` (`cli/src/ups.rs:88-95`) is renamed to `emit_query_failed`
and takes a `detail: String`.

### 4. Collapse the four call sites with exhaustive variant matching

Every site spells out both `UpsQueryError` variants by name. No `Err(_)`
catch-alls in any UPS code path -- the verification step asserts this with
a grep. Adding a third `UpsQueryError` variant in the future will be a
compile error at every site, forcing a re-decision rather than silent
reclassification.

**`cli/src/ups.rs:render_live`** (lines 69-86):

```rust
let parsed = match query_ups(runner, &ups_cfg.name) {
    Ok(p) => p,
    Err(UpsQueryError::InvocationFailed(e)) => {
        return emit_query_failed(json, format!("invocation failed: {e}"));
    }
    Err(UpsQueryError::QueryFailed { exit_code, stderr }) => {
        return emit_query_failed(json, format!("exit {exit_code}: {stderr}"));
    }
};
```

Detail is built explicitly per variant. Do NOT pass `e.to_string()` --
`UpsQueryError`'s own `Display` is prefixed with "upsc query failed:" /
"upsc invocation failed:", and `UpsError::QueryFailed`'s `Display` adds
"upsc query failed: " on top. Threading the variant's `Display` through
would produce a double-prefix like "upsc query failed: upsc query failed
(exit 1): Connection refused". The per-variant `format!` above puts the
underlying signal (the wrapped `CmdError` for invocation, the `exit_code`
+ `stderr` for query) into `detail` cleanly, so the final user-facing
string is "upsc query failed: exit 1: Connection refused" or
"upsc query failed: invocation failed: <CmdError display>". A future third
variant breaks the match and forces re-decision.

**`cli/src/doctor.rs:check_ups_daemon_up`** (lines 625-669): the existing
nested structure (top-level match for spawn, inner match for parse) flattens.
Each variant gets the message it deserves.

```rust
match crate::ups::query_ups(ctx.runner, &ups_cfg.name) {
    Err(UpsQueryError::InvocationFailed(e)) => CheckResult::fail(
        name,
        format!("upsc invocation failed: {e} -- is pkgs.nut on PATH?"),
    ),
    Err(UpsQueryError::QueryFailed { exit_code, stderr }) => CheckResult::warn(
        name,
        format!(
            "upsc {} failed (exit {exit_code}): {stderr} -- \
             check 'systemctl status upsd.service' or verify the UPS name",
            ups_cfg.name
        ),
    ),
    Ok(out) if out.status_flags.is_empty() => CheckResult::warn(
        name,
        format!(
            "upsc {} responded but ups.status is empty -- driver may still be starting",
            ups_cfg.name
        ),
    ),
    Ok(_) => CheckResult::ok(name, format!("upsc {} reachable", ups_cfg.name)),
}
```

The hint now mentions both daemon AND name as possible causes and surfaces
the captured stderr verbatim so the operator can read upsc's own error
wording.

**`cli/src/preflight.rs:check_ups_not_on_battery`** (lines 466-475):

```rust
let parsed = match crate::ups::query_ups(runner, name) {
    Ok(p) => p,
    Err(UpsQueryError::InvocationFailed(_)) => {
        return refuse("upsc invocation failed");
    }
    Err(UpsQueryError::QueryFailed { stderr, .. }) => {
        return refuse(&format!("upsc query failed: {stderr}"));
    }
};
```

The misleading "or unparseable" hedge is gone. The captured stderr is
surfaced directly so the operator sees the actual upsc error.

**`cli/src/tui/probe.rs:probe_ups_for_tui`** (lines 586-609):

```rust
let parsed = match crate::ups::query_ups(runner, name) {
    Ok(p) => p,
    Err(UpsQueryError::InvocationFailed(_))
    | Err(UpsQueryError::QueryFailed { .. }) => {
        return ups_snapshot_query_failed(runner);
    }
};
```

The internal helper `ups_snapshot_daemon_down` (`cli/src/tui/probe.rs:611-623`)
is renamed to `ups_snapshot_query_failed` for naming consistency. Behavior
unchanged. OR-pattern over named variants still triggers a non-exhaustive
match error if a third variant is added later.

### 5. Refresh the clap doc comment in `cli/src/main.rs`

`cli/src/main.rs:92-94` says "distinct error sentinels for the not-enabled
and daemon-down branches." Update to "distinct error sentinels for the
not-enabled and query-failed branches" so `--help` text matches the new
shape.

### 6. Test updates

**`cli/src/parse/upsc.rs` tests** (the `#[cfg(test)] mod tests` block,
lines 157-406):
- Drop the `ok()` helper at lines 161-168 (no longer needed -- callers pass
  `stdout` directly).
- Update 14 surviving `parse_upsc(&ok(stdout)).unwrap()` calls to
  `parse_upsc(stdout)` (no `unwrap`).
- Delete `daemon_down_is_command_failed` (lines 244-257) and
  `parses_daemon_down_fixture` (lines 394-405). These cover the
  runner-integration path that has moved out.

**`cli/src/ups.rs` tests** (the `#[cfg(test)] mod tests` block, line 200+):
- Update `parse_fixture` (line 368) and similar call sites (lines 277-278)
  to pass `stdout` directly.
- Update existing `JsonReport::DaemonDown { error: "daemon_down" }` snapshot
  tests (around line 531-536) to the new `QueryFailed` shape with `detail`.
  Snapshot files in `cli/src/snapshots/` for the affected tests need
  refreshing (`cargo insta accept` after the refactor).
- Rename `snapshot_json_daemon_down` -> `snapshot_json_query_failed` and
  update its preamble.
- Add three `query_ups` tests using `MockRunner` (`cli/src/cmd.rs:950-1079`).
  Use `/* ... */` block-comment preambles per `docs/testing.md:11-24`:
  - `query_ups_returns_query_failed_on_non_zero_exit` -- seed exit=1, empty
    stdout, "Error: Connection failure: Connection refused" stderr; assert
    `Err(UpsQueryError::QueryFailed { exit_code: 1, stderr })` with the
    captured stderr present.
  - `query_ups_returns_invocation_failed_on_missing_mock` -- default
    `MockRunner`, no `UpscQuery` seeded; `MockRunner.run()` yields
    `CmdError::MissingMock` (`cli/src/cmd.rs:1091`); assert
    `Err(UpsQueryError::InvocationFailed(_))`.
  - `query_ups_returns_ok_on_healthy_output` -- seed exit=0 with
    `"ups.status: OL\nbattery.charge: 100\n"`; assert `Ok(out)` with `OL`
    flag and 100% charge.
- Update `render_live_daemon_down_surfaces_typed_error` (line 322) and
  `render_live_non_zero_exit_is_daemon_down` (line 338) to assert on
  `UpsError::QueryFailed { detail }` and pin both the per-variant signal
  in `detail` and the absence of double-prefix in the final `Display`:
  - For the non-zero-exit case (seed exit=1, stderr "Error: Connection
    failure: ..."): assert `detail` starts with `"exit 1: "`, contains
    `"Connection failure"`, and the full `err.to_string()` equals
    `"upsc query failed: exit 1: Error: Connection failure: ..."` (one
    "upsc query failed:" prefix, not two).
  - For the missing-mock case (default `MockRunner`): assert `detail`
    starts with `"invocation failed: "` and the full `err.to_string()`
    starts with `"upsc query failed: invocation failed: "` (single prefix).

**`cli/src/doctor.rs` tests** (around line 1083): update existing
`check_ups_daemon_up` tests to assert the new arm-specific messages
(invocation-failure no longer ends with the systemctl hint; query-failed
message mentions both daemon and name plus the captured stderr).

**`cli/src/preflight.rs` tests** (around lines 1559+): update
`ups_daemon_down_refuses` (line 1563) wording assertion to match
"upsc query failed: <stderr>". The "missing mock output is treated as
daemon-down" test (line 1581) becomes the invocation-failed test and asserts
"upsc invocation failed" wording.

**`cli/src/tui/probe.rs` tests** (around line 646): rename any test
referencing `ups_snapshot_daemon_down` to the new helper name; behavior
assertions stay.

**`cli/tests/support/golden_common.rs`** (integration test crate -- imports
via `braid_cli::*`; see `cli/tests/golden_nixos_25_11.rs:7-8`):
- `upsc_ok` helper (lines 423-435) currently builds a `RawCommandOutput`
  envelope just to call `parse_upsc(&raw)`. Simplify to
  `Some(braid_cli::parse::parse_upsc(&stdout))`.
- `golden_upsc_daemon_down` (lines 538-556) is renamed
  `golden_upsc_query_failed`. Rewrite using `braid_cli::cmd::MockRunner` to
  feed the committed `upsc-daemon-down.stderr` fixture into
  `braid_cli::ups::query_ups`, asserting
  `Err(braid_cli::ups::UpsQueryError::QueryFailed { stderr, .. })` where
  `stderr` contains the fixture text. This now exercises the production
  helper rather than an isolated parser path.

**`tests/cli/braid-status-ups.py`** (live VM canary, `just test-parsers`):
- Lines 56-58 currently assert `parsed_down.get("error") == "daemon_down"`.
  Update to `== "query_failed"` and add a stable-substring assertion on
  `detail` so a regression that loses the captured upsc stderr is caught:
  ```python
  assert parsed_down.get("error") == "query_failed", (
      f"expected error=query_failed, got {parsed_down}"
  )
  detail = parsed_down.get("detail", "")
  # When upsd is stopped, upsc emits "Error: Connection failure: ..." on
  # stderr (see reference/nut/clients/upsc.c). The "Connection failure"
  # substring is the stable, version-independent slice; a regression that
  # drops the captured stderr from `detail` would fail this check.
  assert isinstance(detail, str) and "Connection failure" in detail, (
      f"expected detail to contain upsc stderr 'Connection failure', got {parsed_down}"
  )
  ```
  The comment block at lines 47-49 ("Daemon-down branch", "daemon-down JSON
  shape") should be retitled to "Query-failed branch" / "query-failed JSON
  shape" to match the new model.
- The test continues to stop `upsd.service` to drive the failure, since
  that is still the canonical way to trigger a non-zero `upsc` exit in the
  VM. The plan does not need to add a separate "wrong UPS name" canary.
- If the upstream NUT wording in `reference/nut/clients/upsc.c` is verified
  to be a different stable substring, swap "Connection failure" for that
  during implementation. The point is to assert *something* from upsc's
  own stderr, not to commit to a specific phrase if upstream uses a
  different wording at the pinned version.

### 7. Test conventions for new and rewritten tests

`docs/testing.md:11-24` mandates `/* ... */` block-comment preambles for new
tests. Many existing tests use `//` -- those are grandfathered, not the
standard. Reference example: `lock_retries_busy_close_then_succeeds` in
`cli/src/lock.rs`. All tests added or substantively rewritten by this plan
use the `/* ... */` form:

```rust
/*
 * Intent: <one-line behavior verified>.
 * Why it exists: <regression risk this protects against>.
 * Scenario: <concrete real-world sequence>.
 */
#[test]
fn the_test() { ... }
```

Existing `//`-form tests that are merely retargeted (e.g. signature update
without semantic change) keep their existing comments to minimize churn.

## Files modified

- `cli/src/parse/upsc.rs` (signature, body, doc-comment, tests)
- `cli/src/ups.rs` (new `UpsQueryError` + `query_ups`, reshape `UpsError`,
  reshape `JsonReport`, rename `emit_daemon_down`, render_live update,
  tests, snapshots)
- `cli/src/doctor.rs` (`check_ups_daemon_up` flatten + message refresh)
- `cli/src/preflight.rs` (`check_ups_not_on_battery` typed match, refusal
  wording)
- `cli/src/tui/probe.rs` (`probe_ups_for_tui` typed match, helper rename
  to `ups_snapshot_query_failed`)
- `cli/src/main.rs` (clap doc comment refresh at line 92-94)
- `cli/tests/support/golden_common.rs` (helper simplification + golden test
  rewrite using `braid_cli::*` paths)
- `tests/cli/braid-status-ups.py` (sentinel + detail assertion update,
  comment retitle)

`cli/src/parse/mod.rs:89` (`pub use upsc::parse_upsc;`) needs no change --
the name is re-exported, the signature is at the source.

## Reused utilities

- `MockRunner` and its `with_output` builder (`cli/src/cmd.rs:950-1079`) for
  all new tests. Missing-mock returns `CmdError::MissingMock`
  (`cli/src/cmd.rs:1091`); seeded mocks return the supplied
  `RawCommandOutput`.
- `CmdRequest::UpscQuery { name: String }` (`cli/src/cmd.rs:232-234`).
- `CmdError` enum (`cli/src/cmd.rs:817-823`) -- wrapped via `#[from]` in
  `UpsQueryError::InvocationFailed`. Note: `CmdError::Failed` covers spawn,
  signal-kill, and IO failures; `CmdError::MissingMock` covers MockRunner
  unseeded requests.
- `parse_smartctl` (`cli/src/parse/smartctl.rs:76`) is the in-repo precedent
  for an infallible parser; the new `parse_upsc` shape mirrors it.
- `lock_retries_busy_close_then_succeeds` (`cli/src/lock.rs`) is the
  reference example for `/* ... */` test preambles.

## Verification

1. `just test-rust` -- all unit tests pass. Specifically:
   - `parse/upsc.rs` parser tests (14 surviving) still cover OL/OB/LB,
     fixture round-trips, percent rules, fallback ups.* fields.
   - `ups.rs` gains three `query_ups` tests (QueryFailed, InvocationFailed,
     Ok) with `/* ... */` preambles.
   - Updated insta snapshots accepted (`cargo insta accept`) for the renamed
     `JsonReport::QueryFailed` shape.
   - `doctor.rs`, `preflight.rs`, `tui/probe.rs` UPS tests pass with updated
     assertions on typed match arms and renamed helpers.
   - `cli/tests/support/golden_common.rs` `golden_upsc_query_failed` drives
     `braid_cli::ups::query_ups` via `MockRunner` + the committed stderr
     fixture and surfaces the captured stderr in the error.
2. `just test-parsers` -- the `braid-status-ups` live VM canary asserts
   `error == "query_failed"` and that `detail` is a non-empty string
   carrying the upsc failure text. This is the gate that the previous
   plan revision missed.
3. `cargo build` -- compiles clean. Validates that no external caller of
   `parse_upsc` still passes `&RawCommandOutput`.
4. **No `Err(_)` catch-alls remain in UPS code paths.** Run:
   ```sh
   git grep -nE 'Err\(_\)' cli/src/ups.rs cli/src/doctor.rs \
       cli/src/preflight.rs cli/src/tui/probe.rs
   ```
   -- the only matches that may legitimately remain are unrelated to UPS code
   paths. Every UPS-flow match must spell out
   `UpsQueryError::InvocationFailed(_)` and
   `UpsQueryError::QueryFailed { .. }` by name (single arms or in an
   OR-pattern).
5. `git grep -n 'ParseError::CommandFailed' cli/src/` -- only the parser
   layer's other parsers still emit it; none of the four UPS call sites
   reference it.
6. `git grep -nE 'DaemonDown|daemon_down|SpawnFailed' cli/ tests/`
   -- no surviving Rust identifiers, JSON sentinels, or test-name strings
   using the old names. The cli crate, its integration tests, and the VM
   canary are fully rebranded to "query failed" / "invocation failed".

   The grep deliberately excludes the kebab-case spelling `daemon-down`
   because the committed fixture filename
   `cli/tests/fixtures/nut/upsc-daemon-down.stderr` (also referenced as
   `upsc-daemon-down.stderr` in `tests/capture-ups-fixtures.py:49` and the
   nixos-25.11/nixos-unstable upsc fixture trees) stays. Renaming the
   fixture would require recapturing every UPS fixture, churning git
   history for no semantic gain. The fixture filename is an allowed
   historical artifact; the contract it locks (upsc stderr on a stopped
   daemon) is unchanged. If a future operator wants to rename it, that is
   a separate, optional change.
7. `cargo run -- ups status --help` -- the rendered help text matches the
   refreshed clap doc comment in `cli/src/main.rs:92-94`.

## Out of scope

- `parse_smartctl`'s infallible shape stays as it is. The plan does not
  retrofit other parsers; it only fixes the UPS path where the type/usage
  mismatch is concrete.
- No new `UpsQueryError` variants (e.g. `Malformed { ... }`) added
  speculatively. If a real malformed-exit-0 case ever surfaces, add the
  variant to `UpsQueryError` then; the compiler will surface every
  exhaustive match that needs to be re-decided.
- The grandfathered `//`-style preambles on existing parser tests stay; only
  new and substantively rewritten tests adopt the `/* ... */` form.
- No new live VM canary for "wrong UPS name" -- the existing
  `tests/cli/braid-status-ups.py` already drives non-zero `upsc` exit by
  stopping `upsd.service`, which is sufficient to lock the
  `query_failed` JSON shape against drift.
