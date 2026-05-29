# Pivot: decouple two tests from `cmd.rs`'s test-only `MissingMock` wording

## Context

Two unit tests pin the exact Display string of `CmdError::MissingMock`
(`"mock output missing for request"`) as a hardcoded literal, across a
module boundary. `MissingMock` is a **test-only** variant -- it is produced
only by the test `MockRunner` when no output is seeded (`cli/src/cmd.rs:1454`);
production spawn failures route to `CmdError::Failed(...)`
(`cli/src/cmd.rs:1298`). So a cosmetic reword of `MissingMock`'s
`#[error(...)]` message (`cli/src/cmd.rs:1228`) -- a change with zero
production effect -- would fail two behavior tests in unrelated modules.

A verified sweep of `cli/src/` (ripgrep on the literal plus broader
`CmdError`/`command failed`/`MissingMock` patterns) confirms these are the
**only two** instances of this anti-pattern; no other cross-module test pins a
`CmdError` variant's own wording.

Goal: keep each test's real, behavioral assertions while sourcing the
`CmdError` wording from `CmdError` itself, so the wording lives in exactly one
place. `CmdError` is already imported in both test modules
(`cli/src/ups.rs:300`, used in `cli/src/remove.rs:2213`) -- no new imports.

This is a test-only change. Zero production code is touched.

## Why a pivot (not the finding's literal fix)

The finding proposed simply deleting the `assert_eq!` line from the ups test
and keeping the `!detail.starts_with("invocation failed")` guard. That keeps
the **weaker** of two assertions: exact-string equality already subsumes the
prefix guard, and dropping the equality loses coverage of a real contract --
that `emit_invocation_failed` sets `detail` to the raw `CmdError` display
**verbatim**, with the `-- is pkgs.nut on PATH?` hint added only by the
`Display` impl, never baked into `detail`. That separation is a genuine design
choice, independently relied on by the JSON path
(`cli/src/ups.rs:188-196`) and its test (`cli/src/ups.rs:588-595`, which
asserts the hint is absent from the JSON `detail`).

The ideal pivot keeps that contract but makes it structure-insensitive:
assert `detail` equals `CmdError::MissingMock.to_string()` rather than a
copy of its text.

## Change 1 -- `cli/src/ups.rs` (test `cmd_ups_status_invocation_failure_surfaces_typed_error`)

In the `UpsError::InvocationFailed { detail }` match arm (currently
`cli/src/ups.rs:674-680`), replace the literal assertion **and** the now-redundant
legacy-prefix guard with a single, stronger, decoupled assertion:

```rust
UpsError::InvocationFailed { detail } => {
    assert_eq!(detail, &CmdError::MissingMock.to_string());
}
```

- Decouples from the literal wording: rewording `MissingMock` updates both
  sides together; the test stays green.
- Strengthens coverage: exact equality catches prefix **and** suffix drift in
  `detail` (the old `!starts_with("invocation failed")` guard caught only a
  prefix, and was already redundant under the exact-equality check).
- The `!detail.starts_with("invocation failed")` guard is dropped because exact
  equality strictly subsumes it.

**Leave unchanged** the `err.to_string()` block below it
(`cli/src/ups.rs:683-695`): `display.starts_with("upsc invocation failed: ")`,
`display.contains("-- is pkgs.nut on PATH?")`, and
`!display.contains("query failed")` pin `UpsError::InvocationFailed`'s **own**
Display wording (`cli/src/ups.rs:26`) -- the module's legitimate contract, not
the anti-pattern.

## Change 2 -- `cli/src/remove.rs` (test `plan_preview_renders_soft_warn_above_dry_run_steps`)

The test pins the full rendered first line (`cli/src/remove.rs:2227-2231`),
which embeds the `CmdError` display mid-string. Production builds that body as
`format!("ENOSPC pre-flight check failed: {e}; proceeding anyway")`
(`cli/src/remove.rs:715-717`) and the preview renderer adds the `[warn] `
prefix. Preserve the test's documented intent (pin the full line to catch
notes-before-steps and body-wording regressions) while sourcing only the
embedded `CmdError` display from `CmdError`:

```rust
assert_eq!(
    lines[0],
    format!(
        "[warn] ENOSPC pre-flight check failed: {}; proceeding anyway",
        CmdError::MissingMock
    ),
    "warning must be the first line of the rendered preview; got: {rendered:?}",
);
```

This still pins `remove.rs`'s own framing (`[warn] `, the
`ENOSPC pre-flight check failed: ` prefix, the `; proceeding anyway` suffix)
literally; only the `cmd.rs`-owned substring is now derived.
(`lines[0]` is `&str` and the RHS is `String`; they compare via std's
`PartialEq<String> for &str`.)

## Files

- `cli/src/ups.rs` -- test assertion swap (Change 1).
- `cli/src/remove.rs` -- test assertion swap (Change 2).

No production code, no docs, no fixtures. `CmdError` is already in scope in both
test modules.

## Verification

1. `just test-rust` -- runs `cargo test` for `braid-cli`. Confirm both touched
   tests pass:
   - `cmd_ups_status_invocation_failure_surfaces_typed_error`
   - `plan_preview_renders_soft_warn_above_dry_run_steps`
2. Completeness check -- `rg "mock output missing for request" cli/src` should
   afterward return **only** the enum definition at `cli/src/cmd.rs:1228`.
3. Decoupling spot-check (optional) -- temporarily reword the `#[error(...)]` on
   `CmdError::MissingMock` (`cli/src/cmd.rs:1228`), rerun `just test-rust`,
   confirm both tests still pass, then revert the reword.
