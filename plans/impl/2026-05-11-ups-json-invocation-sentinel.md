# Fix: split `braid ups status --json` invocation-vs-query failure sentinels

## Context

`braid ups status --json` is documented (`manual/commands/ups-status.md`,
`manual/guides/ups.md`) as a stable script surface with distinct error
sentinels per failure class. The codebase keeps a deliberate two-class
split for UPS query failures:

- `UpsQueryError::InvocationFailed` -- `upsc` could not be spawned
  (missing binary, wrapper PATH bug, OS-level fork failure, signal-killed
  child). The operator response is "rebuild NixOS / fix the braid
  wrapper / `pkgs.nut` packaging." `braid doctor` treats this as
  **Fail** (`cli/src/doctor.rs:641-644`).
- `UpsQueryError::QueryFailed { exit_code, stderr }` -- `upsc` ran and
  exited non-zero (stopped `upsd`, unknown UPS name, fatal NUT error).
  The operator response is "fix NUT state -- `systemctl status
  upsd.service`, check the UPS name." `braid doctor` treats this as
  **Warn** (`cli/src/doctor.rs:645-652`).

The `--json` surface in `cli/src/ups.rs:85-93` flattens both into a
single sentinel `error: "query_failed"`, with the class only embedded
inside the free-form `detail` string (`"invocation failed: {e}"` vs
`"exit {n}: {stderr}"`). Scripts consuming `.error` must substring-match
English prose in `.detail` to recover a distinction the typed enum
already carries and the doctor already projects as a severity boundary.

This plan adds a third sentinel `error: "invocation_failed"` so the JSON
contract matches the typed-error split and the doctor severity split.

Outcome: scripts wrapping `braid ups status --json` can decide between
"your CLI install / wrapper / packaging is broken" and "your NUT state
is unhealthy" by reading `.error` alone, without parsing `.detail`.

## Approach

Local change to `cli/src/ups.rs` plus mirror updates in two manuals and
two snapshot/structural Rust tests, plus one new VM assertion in the
existing CLI canary. No new modules, no API beyond the `--json`
sentinel surface, no change to typed Rust errors visible to other
callers, no doctor / preflight / TUI changes -- those consumers already
match on `UpsQueryError` variants directly and are unaffected.

### Code changes (`cli/src/ups.rs`)

1. Extend `ErrorReport` (`cli/src/ups.rs:64-71`) with a third variant:
   ```rust
   #[serde(rename = "invocation_failed")]
   InvocationFailed { detail: &'a str },
   ```
   Keep `query_failed` for the non-zero-exit branch. Keep
   `ups_not_enabled`.

2. Add a parallel emitter helper next to the existing
   `emit_query_failed` (`cli/src/ups.rs:116-124`):
   ```rust
   /// Emit the `invocation_failed` JSON sentinel for `--json`, or wrap
   /// the detail in the existing human `QueryFailed` for stderr. Kept
   /// separate from `emit_query_failed` so the two sentinels stay
   /// independent at the JSON surface even though they share an exit
   /// code and human-error shape.
   fn emit_invocation_failed(json: bool, detail: String) -> Result<(), UpsError> {
       if json {
           emit_json(&JsonReport::Error(ErrorReport::InvocationFailed {
               detail: &detail,
           }))?;
           return Err(UpsError::QueryFailedJsonReported);
       }
       Err(UpsError::QueryFailed { detail })
   }
   ```
   - Reuses `UpsError::QueryFailedJsonReported` so the existing
     `main.rs:812-813` exit-1 wiring continues to apply unchanged --
     both `--json` failure branches stay stdout-only with exit 1.
   - Continues to wrap human-mode under the existing `UpsError::QueryFailed`
     so `print_cli_error` keeps printing the same "invocation failed: ..."
     stderr line operators already see. The human path is unchanged.
   - A parallel helper (rather than generalizing `emit_query_failed`)
     mirrors the existing pattern; the two-line duplication is cheaper
     to read than a closure-based unifier.
   - The `///` doc comment is required by `AGENTS.md:133-148` for new
     top-level Rust CLI items. Implementer: trim if it restates the
     signature; the why-this-helper-exists framing is the load-bearing
     bit.

3. Route `UpsQueryError::InvocationFailed` in `cmd_ups_status`
   (`cli/src/ups.rs:85-93`) through the new helper:
   ```rust
   Err(UpsQueryError::InvocationFailed(e)) => {
       return emit_invocation_failed(json, format!("invocation failed: {e}"));
   }
   ```
   Leave the `QueryFailed` arm exactly as it is.

4. Update the module-level doc comment (`cli/src/ups.rs:1-9`) to note
   three error sentinels (not two): `query_failed` (non-zero exit),
   `invocation_failed` (spawn failure), `ups_not_enabled` (config).

### Documentation changes

5. `manual/commands/ups-status.md` -- update the error-shape table
   (lines 71-75) to add a row for `invocation_failed`:
   ```
   | UPS invocation failed (upsc could not run -- missing on PATH, killed by signal, or other runner-level failure) | `{"error": "invocation_failed", "detail": "invocation failed: ..."}` | 1 |
   ```
   Keep the existing two rows. Reorder so the table reads enabled-path
   → query_failed → invocation_failed → not_enabled, matching severity
   intuition.

   The condition wording is deliberately broader than "spawn failure":
   `UpsQueryError::InvocationFailed` wraps `CmdError`
   (`cli/src/ups.rs:30-34`), which covers spawn failures, signal-killed
   children, stdin IO errors, and request/mode mismatches. Narrowing
   the doc to "PATH-missing" would misrepresent what consumers will see
   in `.detail`.

   Update the adjacent prose at `manual/commands/ups-status.md:77` from
   "For the three cases above" to "For these cases" (or "For the four
   cases above") so the count stays correct after the new row.

6. `manual/guides/ups.md` -- update the error-shape table (lines 81-85)
   identically. Both tables stay in sync because they share copy. No
   adjacent prose count to fix in this file -- only the table.

### Test changes

#### Rust (unit, in `cli/src/ups.rs` tests module)

All three new tests below get the standard `// Intent` / `// Why it
exists` / `// Scenario` preamble required by `AGENTS.md:187-195`. The
preamble form is what existing ups.rs tests already use (see e.g.
`cli/src/ups.rs:244-249, 397-401, 418-422`).

7. **Snapshot test** -- mirror `snapshot_json_query_failed`
   (`cli/src/ups.rs:710-716`):
   ```rust
   // Intent: invocation-failed --json serializes to the
   // `invocation_failed` sentinel with detail.
   // Why it exists: scripts key off `.error == "invocation_failed"`
   // to distinguish a broken braid wrapper / missing nut package
   // from `query_failed` (live NUT state); a snapshot pins the
   // exact JSON shape against accidental sentinel renames.
   // Scenario: `braid ups status --json` when `upsc` cannot be
   // spawned (e.g. wrapper PATH bug or nut packaging error).
   #[test]
   fn snapshot_json_invocation_failed() {
       let payload = JsonReport::Error(ErrorReport::InvocationFailed {
           detail: "invocation failed: upsc: No such file or directory",
       });
       snap_json!(&payload);
   }
   ```
   Generates `cli/src/snapshots/snapshot_json_invocation_failed.snap`
   pinning the sentinel string and the detail key.

8. **Structural assertion** -- mirror
   `json_query_failed_has_sentinel_error_and_detail`
   (`cli/src/ups.rs:402-410`):
   ```rust
   // Intent: the `invocation_failed` JSON payload carries both the
   // sentinel and the captured detail.
   // Why it exists: cheap structural guard against a future refactor
   // that drops the `detail` field or renames the sentinel without
   // updating the doc.
   // Scenario: unit-level mirror of the snapshot test, so a `detail`
   // shape regression fails loudly without an insta accept.
   #[test]
   fn json_invocation_failed_has_sentinel_error_and_detail() {
       let payload = JsonReport::Error(ErrorReport::InvocationFailed {
           detail: "invocation failed: upsc: No such file or directory",
       });
       let text = serde_json::to_string_pretty(&payload).unwrap();
       assert!(text.contains("\"invocation_failed\""));
       assert!(text.contains("No such file or directory"));
   }
   ```

9. **Command-level coverage** -- extend or duplicate
   `cmd_ups_status_invocation_failure_surfaces_typed_error`
   (`cli/src/ups.rs:480-497`) with a `json=true` variant that asserts
   the returned error is `UpsError::QueryFailedJsonReported` (so the
   main shell exits 1 quietly):
   ```rust
   // Intent: cmd_ups_status under --json routes invocation failure
   // through QueryFailedJsonReported.
   // Why it exists: pins the contract main.rs depends on at
   // main.rs:812-813 -- the JSON-reported sentinel is what tells the
   // CLI shell to exit 1 without printing a duplicate human stderr
   // line.
   // Scenario: MockRunner with no UpscQuery mock seeded simulates a
   // spawn failure (CmdError::MissingMock) under --json.
   #[test]
   fn cmd_ups_status_invocation_failure_json_returns_already_reported() {
       let runner = MockRunner::default();
       let dir = tempfile::tempdir().unwrap();
       let cfg = ups_write_config(&dir, "ups");
       let err = cmd_ups_status(&runner, &cfg, true).expect_err("query failure expected");
       assert!(matches!(err, UpsError::QueryFailedJsonReported), "got {err:?}");
   }
   ```
   This pairs with the existing `emit_query_failed_json_returns_already_reported`
   (`cli/src/ups.rs:533-540`) and the existing non-JSON invocation test,
   completing the 2x2 matrix.

#### VM (`tests/cli/braid-status-ups.py`)

10. Add a third sub-section after the existing query-failed block
    (after line 73). Use the same wrapper-extraction trick as
    `tests/cli/braid-idle.py:50-75` -- read the wrapper script, regex
    out the unwrapped `/nix/store/...braid-cli/bin/braid`, and invoke
    it with `PATH=/nonexistent` so `upsc` cannot be found by
    `std::process::Command::new("upsc")`. Mirror the query-failed
    block's pattern of redirecting stdout / stderr separately so we
    can assert the `--json` stdout-only contract end-to-end:

    ```python
    # --- Invocation-failed branch ---
    # Force upsc to fail to spawn by running the unwrapped braid with a
    # PATH that does not include nut. This pins the "invocation_failed"
    # sentinel that distinguishes "your braid wrapper / nut package is
    # broken" from "your upsd is down" (the latter is the
    # `query_failed` block above). Also pins the stdout-only contract:
    # --json must not print a redundant human error to stderr.
    braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()
    wrapper_source = machine.succeed(f"cat {braid_wrapped_path}")
    m = re.search(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', wrapper_source)
    assert m, f"could not locate unwrapped braid in wrapper:\n{wrapper_source}"
    unwrapped_braid = m.group(1)

    exit_code = machine.execute(
        f"PATH=/nonexistent {unwrapped_braid} ups status --json "
        ">/tmp/ups_if.out 2>/tmp/ups_if.err"
    )[0]
    assert exit_code != 0, (
        "braid ups status --json must exit non-zero on invocation failure; got 0"
    )
    out_if = machine.succeed("cat /tmp/ups_if.out")
    err_if = machine.succeed("cat /tmp/ups_if.err")
    parsed_if = json.loads(out_if)
    assert parsed_if.get("error") == "invocation_failed", (
        f"expected error=invocation_failed, got {parsed_if}"
    )
    detail_if = parsed_if.get("detail", "")
    assert isinstance(detail_if, str) and "invocation failed" in detail_if, (
        f"expected detail to contain 'invocation failed', got {parsed_if}"
    )
    assert err_if == "", (
        f"expected empty stderr in --json invocation-failed, got: {err_if!r}"
    )
    ```
    Add `import re` at the top of the file (line 9 area).

    The block does not depend on upsd state -- the failure is at the
    `Command::new("upsc")` spawn site, independent of NUT. Place it
    after the query-failed block (which has already stopped upsd) so
    the file reads top-to-bottom by failure-severity; order is not
    load-bearing otherwise.

### Notes on what is NOT changing

- `UpsQueryError` and `UpsError` Rust shapes are unchanged. Callers in
  `cli/src/preflight.rs:466-474` and `cli/src/doctor.rs:640-661` keep
  matching on the typed variants and remain correct.
- `UpsError::QueryFailedJsonReported` is intentionally reused for both
  JSON failure branches; renaming it would be churn for no consumer.
- No ADR update. `docs/decisions/020-ups-integration.md` only scopes
  `--json` as a stable script surface; it does not enumerate sentinels,
  and the manuals are the authoritative reference for the shape.
- The `_old/fix-ups-doctor-spawn-failure-severity.md` plan is unrelated
  and already shipped (doctor severity split lives in doctor.rs); this
  fix is its `--json` analogue but does not depend on or modify it.

## Critical files

- `cli/src/ups.rs` -- the `ErrorReport` enum, the `cmd_ups_status`
  match, the emit helpers, and the unit tests / snapshots.
- `cli/src/snapshots/snapshot_json_invocation_failed.snap` -- new
  snapshot file generated on first `cargo insta accept`.
- `manual/commands/ups-status.md` -- error-shape table (line 71).
- `manual/guides/ups.md` -- error-shape table (line 81).
- `tests/cli/braid-status-ups.py` -- new spawn-failure subtest.

## Verification

1. `just test-rust` -- new and existing Rust unit tests pass; the
   snapshot file is created on first run via `cargo insta` (run
   `cargo insta accept` if reviewing locally). Existing snapshots for
   `query_failed` and `not_enabled` must not change.
2. `just test-vm braid-status-ups` -- the live NixOS VM canary
   exercises all three JSON error branches (`query_failed`,
   `invocation_failed`, `ups_not_enabled`) against real `upsc`. With
   `-v` if the spawn-failure block is the one that fails, to inspect
   the captured stdout.
3. Manual sanity (optional, on the VM):
   ```sh
   sudo systemctl stop upsd.service
   braid ups status --json   # → error: "query_failed"
   PATH=/nonexistent /nix/store/.../braid-cli/bin/braid ups status --json
                              # → error: "invocation_failed"
   ```
4. Confirm `braid doctor --json` is unchanged (it has its own JSON
   shape, separate from the ups.rs sentinels) by spot-checking
   `tests/module/braid-doctor-ups.py` still passes.
