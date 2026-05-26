# Pivot: single `upsc` query boundary (dedup the exit-code gate)

## Context

`query_ups` (`cli/src/ups.rs:54`) and `probe_ups_for_tui`
(`cli/src/tui/probe.rs:744`) each independently run
`CmdRequest::UpscQuery`, check `exit_status != 0`, then call `parse_upsc`.
That's two copies of the "what counts as a usable `upsc` result" gate, which
can drift (the finding's concern). The TUI duplicates it because it *also*
needs the verbatim `upsc` stdout -- stored in `UpsSnapshot.raw_text`
(`cli/src/tui/model.rs:184`) and rendered in the Browse "NUT Variables" panel
(`cli/src/tui/browse/view.rs:132`) -- and `query_ups` throws that stdout away,
returning only the parsed `UpscOutput`. So the finding's literal fix ("just
call `query_ups`") would silently empty that panel, and no existing test would
catch it.

The doc comment at `cli/src/tui/effect.rs:35` already *claims* the TUI probe
runs "through `query_ups`". Today that's false. This pivot makes it true.

## Approach

Make `query_ups` the one boundary and have it return both the raw stdout and
the parsed model in a small bundle. Parsed-only callers project to `.parsed`;
the TUI uses both fields. `UpscOutput`, `parse_upsc`, and the `--json` contract
are untouched, so there is no parser-fixture or VM-test surface.

### `cli/src/ups.rs`

Add the bundle struct (doc-commented per the repo's pub-item rule):

```rust
/// Result of a successful `upsc` query: the verbatim stdout plus the parsed
/// model. Bundled so the single query boundary serves both the parsed-only
/// callers (status/doctor/preflight via `.parsed`) and the TUI Variables
/// panel (via `.raw_stdout`) without widening the serialized
/// `UpscOutput`/`--json` model or duplicating the exit-code gate.
#[derive(Debug)]
pub struct UpscQueried {
    pub raw_stdout: String,
    pub parsed: UpscOutput,
}
```

Derive `Debug` only: the Err-arm tests call `query_ups(...).expect_err(...)`
(`ups.rs:599`, `ups.rs:622`, `golden_common.rs:617`), and
`Result::<T, E>::expect_err` bounds the `Ok` type `T` by `Debug` -- without it
`just test-rust` will not compile. Deliberately **no** `Serialize` (and no
`Clone`/`PartialEq` -- nothing clones or equality-compares the bundle): the
struct is internal and must never reach `--json`; only `.parsed` is serialized,
via `JsonReport::success`.

Change `query_ups` to `-> Result<UpscQueried, UpsQueryError>`. The gate is
unchanged; parse before moving stdout so there's no clone:

```rust
let raw = runner.run(&CmdRequest::UpscQuery { name: name.to_owned() })?;
if raw.exit_status != 0 {
    return Err(UpsQueryError::QueryFailed {
        exit_code: raw.exit_status,
        stderr: raw.stderr.trim().to_owned(),
    });
}
let parsed = parse_upsc(&raw.stdout);
Ok(UpscQueried { parsed, raw_stdout: raw.stdout })
```

- `cmd_ups_status` (line ~133): `Ok(p) => p` becomes `Ok(q) => q.parsed`.
  Everything downstream (`JsonReport::success(&parsed)`, `format_human`) still
  operates on the parsed `UpscOutput`, so `--json` output is byte-identical.
- Unit test `query_ups_returns_ok_on_healthy_output` (line ~638): assert on
  `out.parsed.status_flags` / `out.parsed.battery.charge_pct`. The two
  Err-arm tests (lines ~595, ~619) are unchanged.

### `cli/src/doctor.rs` (line ~1217)

Only the guarded `Ok` arm changes: `Ok(out) if out.status_flags.is_empty()`
becomes `Ok(q) if q.parsed.status_flags.is_empty()`. The catch-all
`Ok(_) => CheckResult::ok(...)` (line ~1237) discards the value, so it stays
as-is. Err arms unchanged.

### `cli/src/preflight.rs` (line ~573)

`Ok(p) => p` becomes `Ok(q) => q.parsed`. The subsequent `parsed.is_critical()`
/ `parsed.is_on_battery()` checks are unchanged. Err arms unchanged.

### `cli/src/tui/probe.rs` (lines ~744-768)

Replace the inline run + gate + parse with a single call to the boundary:

```rust
pub fn probe_ups_for_tui<R: CommandRunner>(runner: &R, name: &str) -> UpsSnapshot {
    let q = match crate::ups::query_ups(runner, name) {
        Ok(q) => q,
        Err(_) => return ups_snapshot_query_failed(runner),
    };
    UpsSnapshot {
        flags: q.parsed.status_flags.clone(),
        battery_charge_pct: q.parsed.battery.charge_pct,
        runtime_secs: q.parsed.battery.runtime_secs,
        load_pct: q.parsed.load_pct,
        watts_estimated: q.parsed.watts_estimated(),
        raw_text: q.raw_stdout,
        daemon: probe_daemon_status(runner, UPS_DAEMON_UNIT),
        probed_at: Instant::now(),
    }
}
```

The `Err(_)` arm catches both `InvocationFailed` (runner-level error) and
`QueryFailed` (non-zero exit), preserving today's collapse of both into
`ups_snapshot_query_failed`. Remove the now-unused `use crate::parse::parse_upsc;`
import (line 8 -- it was referenced only here). Keep the `CmdRequest` import: it
is a whole-enum import used throughout `probe.rs`, and the probe test at line
~2847 still seeds `CmdRequest::UpscQuery` (which `query_ups` issues unchanged),
so the mock helper needs no edit.

### `cli/src/tui/effect.rs` (line ~35)

Update the `ProbeUps` doc comment so it accurately describes the now-real path:
the probe runs `upsc` through `query_ups`, which returns the raw stdout
alongside the parsed model.

## Tests

The dedup must preserve `raw_text`, which **no current test covers** -- exactly
why the finding's "verify the existing probe.rs tests still pass" was
insufficient (they assert typed fields and the two fail-closed branches, never
`raw_text`).

- Extend `probe_ups_populates_typed_fields_on_success` (`probe.rs` line ~2875)
  to assert `snap.raw_text` equals the `upsc` stdout fed to the mock.
- Add `assert!(snap.raw_text.is_empty())` to both fallback tests:
  `probe_ups_falls_back_on_invocation_failure` (~2914) and
  `probe_ups_falls_back_on_query_failure` (~2941).
- Must stay green unchanged: the Err-arm unit tests in `ups.rs` and the
  integration assertion at `cli/tests/support/golden_common.rs:616` (it matches
  `UpsQueryError::QueryFailed` only, so the `Ok`-payload change does not touch
  it).

## Verification

- `just test-rust` -- covers the `ups.rs` unit tests, the `probe.rs` TUI tests,
  and the `golden_common.rs` integration check.
- A build (via the test run, or `cargo build`) to confirm no unused-import
  warnings in `probe.rs`.
- No NixOS/VM tests and no fixture refresh: `UpscOutput`, `parse_upsc`, and the
  `braid ups status --json` shape are untouched, so the parser-compat fixtures
  and VM lanes are unaffected.

## Out of scope / deliberately unchanged

- `UpscOutput` and the `braid ups status --json` contract.
- `cli/src/parse/upsc.rs:13` ("classified by `crate::ups::query_ups`") stays
  accurate -- `query_ups` remains the boundary.
- No new public function; `query_ups` keeps its name and role. The only new
  public item is the `UpscQueried` struct.
