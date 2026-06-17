# Plan: make `braid ups status` happy-path fast-lane testable

## Context

A code-review finding (Low / Testing) flagged that the healthy success path of
`cmd_ups_status` is exercised only by the slow aarch64-darwin VM lane
(`tests/cli/braid-status-ups.py`). Every Rust unit test for `cmd_ups_status`
drives an error branch (`cli/src/ups.rs:800,834,849,874`); none drives the
function to `Ok(UpsStatusOutcome::Done)`, and -- more importantly -- no fast
test pins which body (`--json` vs human) the command emits for a given `json`
flag. That selection IS the user-facing `--json` contract, and it lives at the
`cmd_ups_status` boundary (the `if json` dispatch at `ups.rs:155-159`).

An earlier draft of this plan extracted a pure `render_status(name, parsed,
json) -> String` and tested it directly. Review correctly rejected that: a
pure-renderer test proves the helper's branch but NOT the command boundary. A
regression where `cmd_ups_status` forwarded `!json`, ignored `json`, or printed
the wrong body would still pass an `Ok(Done)`-only test (both modes return
`Done`) and a renderer-only test (it bypasses `cmd_ups_status`). The mode
selection would stay VM-only -- exactly the gap the finding is about.

The fix is to make `cmd_ups_status`'s output capturable in the fast lane via
writer injection, which is already braid's idiom for read-only command output:
the sibling `status` command uses `out: &mut dyn std::io::Write`
(`status.rs#emit_paused_balance_warning`, `status.rs:954`), writes best-effort
with `.ok()` (`status.rs:961-964`), and its test captures a `Vec<u8>` buffer
(`status.rs#emit_paused_balance_warning_writes_to_buffer`, `status.rs:3746-3749`).
(The planner/`execute()` split in `remove_missing.rs` is the ADR 022 dry-run
pattern, reserved for *mutating* commands -- not applicable to a status read.)
Intended outcome: the most common invocation's wiring AND its `--json`/human
selection are pinned by fast unit tests that capture real command output; the
VM lane keeps owning only genuine end-to-end concerns (live NUT, exit codes,
`--json` stderr silence).

## Production change -- `cli/src/ups.rs`

Split `cmd_ups_status` into a thin public wrapper (unchanged signature, so
`main.rs:1110` and the TUI/doctor/preflight `query_ups` consumers are untouched)
over a writer-injected private core. Thread `out: &mut dyn std::io::Write`
through the existing emit helpers; reuse `emit_json` and `format_human` as-is
(no new `render_status`). Writes are best-effort `.ok()` per braid convention.

```rust
/// Public entry for `braid ups status`: renders to real stdout. Thin wrapper
/// over the writer-injected core so the `--json`/human selection is capturable
/// in fast tests without an `out` parameter leaking into main.rs's call site.
pub fn cmd_ups_status<R: CommandRunner>(
    runner: &R,
    config_path: &Path,
    json: bool,
) -> Result<UpsStatusOutcome, UpsError> {
    cmd_ups_status_to(runner, config_path, json, &mut std::io::stdout())
}

/// Writer-injected core so tests assert the user-facing mode selection at the
/// command boundary by capturing `out` (mirrors `status::emit_paused_balance_warning`).
fn cmd_ups_status_to<R: CommandRunner>(
    runner: &R,
    config_path: &Path,
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<UpsStatusOutcome, UpsError> {
    let config = config_read(config_path)?;
    let Some(ups_cfg) = config.ups() else {
        return print_not_enabled(json, out);
    };
    let parsed = match query_ups(runner, &ups_cfg.name) {
        Ok(q) => q.parsed,
        Err(UpsQueryError::InvocationFailed(e)) => return emit_invocation_failed(json, e, out),
        Err(UpsQueryError::QueryFailed { exit_code, stderr }) => {
            return emit_query_failed(json, format!("exit {exit_code}{}", detail_suffix(&stderr)), out);
        }
    };
    if json {
        emit_json(&JsonReport::success(&parsed), out)?;
    } else {
        write!(out, "{}", format_human(&ups_cfg.name, &parsed)).ok();
    }
    Ok(UpsStatusOutcome::Done)
}
```

Thread `out` through the four emit helpers (append `out: &mut dyn std::io::Write`,
swap `print!`/`println!` -> `write!`/`writeln!(out, ...).ok()`); keep their
existing return types and control flow:
- `emit_json(payload, out)` -- keeps `Result<(), UpsError>` for the `Serialize`
  error; `writeln!(out, "{text}").ok()` for the write.
- `print_not_enabled(json, out)` -- json arm `emit_json(..NotEnabled.., out)?`;
  human arm `writeln!(out, "...").ok()`; returns `Ok(Done)`.
- `emit_query_failed(json, detail, out)` / `emit_invocation_failed(json, error, out)`
  -- json arm `emit_json(.., out)?` then `Ok(JsonErrorReported)`; human arm
  returns the typed `Err` (unchanged -- human errors still go to stderr via
  main.rs, nothing written to `out`).

Notes:
- **Output bytes are preserved exactly.** `writeln!(out, "{x}").ok()` emits the
  same bytes as today's `println!("{}", x)` (text + one LF); `write!(out, "{}",
  format_human(..))` matches `print!`. This is now guarded at the seam itself by
  the byte-exact `assert_eq!`s in tests 1-2 (not only by helper snapshots and VM
  substring checks); existing render snapshots and the VM test remain as defense
  in depth.
- **`.ok()` not panic.** Matches `status.rs`; drops today's implicit
  panic-on-broken-pipe from the `print!` macros. No new `Io` error variant.
- **`&mut dyn Write`, not generic `W`** -- avoids monomorphizing the core +
  four helpers; matches `status.rs`/`lock.rs`. The wrapper passes `&mut
  std::io::stdout()` as a temporary, exactly like `main.rs:972`'s `&mut
  std::io::stderr()`.

## Tests to add -- `cli/src/ups.rs` `mod tests`

Each test carries the file's `// Intent: / // Why it exists: / // Scenario:`
preamble (ASCII only). Use the existing `ups_query_healthy_minimal()` fixture
and `String::from_utf8(buf)` capture (mirroring
`emit_paused_balance_warning_writes_to_buffer`). `MockRunner::with_output`
replays (`cmd.rs:1560` clones, never removes), so one seeded runner serves both
modes in a single test.

1. **`cmd_ups_status_healthy_selects_mode_and_returns_done`** -- the boundary
   guard (replaces the old `..._returns_done` + `render_status_selects_...`).
   Seed `MockRunner::default().with_output(req, output)` from
   `ups_query_healthy_minimal()`; `ups_write_config(&dir, "ups")`. Build the
   expected model in-test from the same seam the command uses:
   `let parsed = query_ups(&runner, "ups").unwrap().parsed;` (`with_output`
   replays, so this and the two command calls all resolve). Then assert the
   captured bytes EXACTLY, not via `contains`/parse:
   - human: `cmd_ups_status_to(&runner, &cfg, false, &mut buf)` -> `Ok(Done)`;
     `assert_eq!(String::from_utf8(buf)?, format_human("ups", &parsed))`.
   - json: fresh `buf`; `cmd_ups_status_to(&runner, &cfg, true, &mut buf)` ->
     `Ok(Done)`; `assert_eq!(String::from_utf8(buf)?, format!("{}\n",
     serde_json::to_string_pretty(&JsonReport::success(&parsed)).unwrap()))`.
   Why: exact equality pins both the mode selection AND the byte-preservation
   claim at the seam -- a `!json` typo, an ignored `json`, a swapped body, a
   dropped JSON trailing newline, or an extra appended line all fail here.
   Comparing against the pure helpers (`format_human`, `to_string_pretty(&JsonReport::success(..))`)
   rather than re-typed literals keeps the test honest: it asserts "the boundary
   emits the helper output verbatim." Residual circularity -- a change to
   `emit_json`'s own format (compact vs pretty) would move both sides together --
   is owned by the independent `snapshot_json_*` records (`ups.rs:1053,1101`) and
   `format_human`'s snapshots (`ups.rs:962`+), so the two layers compose to full
   coverage. (The two equalities also imply `human != js`, so no separate
   distinctness assert is needed.)

2. **`cmd_ups_status_not_enabled_selects_mode_and_returns_done`** -- companion;
   `ups_write_config_without_ups(&dir)`, `MockRunner::default()` (returns before
   `query_ups`). Assert exact captured bytes for both modes:
   - human: `cmd_ups_status_to(&runner, &cfg, false, &mut buf)` -> `Ok(Done)`;
     `snap!(String::from_utf8(buf)?)` -- an insta snapshot (file idiom, no
     re-typed literal) that ALSO newly pins the human not-enabled hint wording in
     the fast lane (today only a VM substring check at
     `braid-status-ups.py:70-74`).
   - json: fresh `buf`; `cmd_ups_status_to(&runner, &cfg, true, &mut buf)` ->
     `Ok(Done)`; `assert_eq!(String::from_utf8(buf)?, format!("{}\n",
     serde_json::to_string_pretty(&JsonReport::Error(ErrorReport::NotEnabled)).unwrap()))`.
   Why: writer injection lets this branch's mode selection AND exact bytes be
   pinned too, so the earlier draft's "not-enabled json/human arms are not
   distinguishable -- out of scope" caveat is gone; both `Done`-returning branches
   are now fast-lane covered end to end, byte-exact.

The error branches keep their existing coverage: outcome via
`emit_query_failed_json_returns_json_error_reported` /
`emit_invocation_failed_json_returns_json_error_reported`, sentinel shape via the
`JsonReport`/snapshot tests. Their signatures gain `out`, so update those call
sites to pass a throwaway `&mut Vec::new()` (or assert the captured sentinel,
optional -- not required by this finding).

## New fixture -- `cli/src/test_fixtures/ups.rs`

Add a sibling to `ups_write_config` (with a `///` per AGENTS.md). Do not reuse
`doctor.rs`'s `config_without_ups` -- it yields a `&str`, not a path writer, and
belongs to a different scope.

```rust
/// Config without a `ups` block so cmd_ups_status tests can drive the
/// not-enabled early return through the on-disk boundary.
pub(crate) fn ups_write_config_without_ups(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"mount_point":"/mnt/storage"}"#).unwrap();
    path
}
```

Wire the new name into the re-export list in `cli/src/test_fixtures.rs:238` and
the test-module `use crate::test_fixtures::{...}` import in `ups.rs:303-306`.

## Files modified

- `cli/src/ups.rs` -- public wrapper + private `cmd_ups_status_to`, thread `out`
  through the four emit helpers, add tests 1-2, update the existing
  `emit_*`-helper call sites in tests to pass `out`.
- `cli/src/test_fixtures/ups.rs` -- add `ups_write_config_without_ups`.
- `cli/src/test_fixtures.rs` -- add the new fixture to the re-export list.

## Deliberately out of scope

- Writer-injected captured assertions on the json error-branch sentinels
  (already covered by isolation + outcome tests; the finding is about the
  success/not-enabled mode wiring).
- A new `UpsError::Io` variant -- writes stay best-effort `.ok()` per braid's
  injected-writer convention.

## Verification

- `just test-rust` (or `cargo test -p braid-cli ups`) -- the two new tests pass
  (the not-enabled human snapshot needs one `cargo insta accept`); the existing
  `ups.rs` suite (render snapshots, JSON sentinels, error branches) stays green.
  The new byte-exact `assert_eq!`s confirm `cmd_ups_status_to` preserved output
  bytes at the boundary.
- Confirm no output regression: the VM end-to-end guard
  `tests/cli/braid-status-ups.py` (human at line 23, `--json` at line 37) is
  unchanged and remains the live-NUT / exit-code / stderr-silence backstop.
- No ASCII-rule impact: the writer-injected core emits the same user-facing
  bytes as before; new doc comments and tests are exempt from
  `scripts/docs/check-output-ascii.py` (comments/tests).
