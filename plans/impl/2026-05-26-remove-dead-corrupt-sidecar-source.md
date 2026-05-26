# Plan: remove dead `CorruptSidecarError::source()`

## Context

`CorruptSidecarError` is the failure surface for the discover `--write` corrupt-sidecar
gate (`cli/src/membership.rs:587-612`). It exposes three inherent accessors:

- `target(&self) -> &Path` -- used.
- `into_source(self) -> std::io::Error` -- used.
- `source(&self) -> &std::io::Error` -- **never called**, kept alive only by an
  `#[allow(dead_code)]` attribute.

The borrowing `source()` accessor was added speculatively for symmetry when the type was
introduced (commit `807b20a4`, "fix(discover): snapshot corrupt pool.json before rebuild")
and wore `#[allow(dead_code)]` from the first commit -- it was born dead. The `#[allow]`
suppresses the compiler signal that would otherwise flag it, leaving confusing surface on
an error type that is otherwise tightly scoped to one call site.

Outcome: delete the dead accessor so the type carries only the two accessors its single
consumer actually uses, and the suppressed `dead_code` lint disappears with it.

## Verified facts (read-only investigation)

- Sole consumer: `cli/src/discover.rs:617-622` maps `CorruptSidecarError` into
  `DiscoverWriteError::CorruptSidecarFailed` using `e.target()` (path string) and
  `e.into_source()` (move the io::Error). It never calls `source()`.
- `rg '\.source\(\)' --type rust` across the whole repo returns **no call site**.
- `CorruptSidecarError` derives only `Debug` (`membership.rs:590`) and has a single
  inherent `impl` block. No `impl Display`, no `impl std::error::Error`, no `thiserror`.
  So `source()` is **not** part of any error-chaining/trait contract (and its
  `&self -> &std::io::Error` signature would not satisfy `Error::source` anyway).
- The type is `pub(crate)`, so no integration test in `cli/tests/` can reference it;
  `rg` confirms none do.

## Change

Single edit in `cli/src/membership.rs`. Delete the doc comment, the `#[allow(dead_code)]`
attribute, and the method body (current lines 602-606), plus the now-redundant blank line,
leaving exactly one blank line between `target()` and `into_source()`.

Before:

```rust
impl CorruptSidecarError {
    /// Sidecar path attempted when the error occurred.
    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    /// Borrow the underlying I/O error without consuming the wrapper.
    #[allow(dead_code)]
    pub(crate) fn source(&self) -> &std::io::Error {
        &self.source
    }

    /// Move the underlying I/O error into a caller-owned error variant.
    pub(crate) fn into_source(self) -> std::io::Error {
        self.source
    }
}
```

After:

```rust
impl CorruptSidecarError {
    /// Sidecar path attempted when the error occurred.
    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    /// Move the underlying I/O error into a caller-owned error variant.
    pub(crate) fn into_source(self) -> std::io::Error {
        self.source
    }
}
```

No other edits. In particular:

- **No formatter run** (per AGENTS.md "Formatting": narrow hand edit only).
- **No doc updates.** The `plans/impl/*.md` references are archival snapshots of past work,
  not living contracts; they are not retro-edited.
- **No new dead-code fallout.** The `source` *field* (`membership.rs:593`) stays live --
  it is still read by `into_source()` -- so removing the accessor introduces no new
  `dead_code` warning.

## Why delete rather than wire it up

The obvious alternative -- implement `std::error::Error` so `source()` becomes a real
chaining hook -- is rejected: the type is consumed and flattened into `DiscoverWriteError`
at its only call site, never boxed or used as `dyn Error`. Adding a trait impl would be
new speculative surface, the opposite of the simplicity goal that motivates this change.

## Verification

Behavior is unchanged (dead code removal), so unit-level compilation/test coverage is
sufficient -- no VM tests, no parser fixture refresh (nothing parser-critical changes).

1. `just test-rust` -- compiles the CLI crate and runs `cargo test`. Confirms the crate
   still builds with no new warnings and the discover corrupt-sidecar tests still pass.
2. Optional sanity: `rg '\.source\(\)' --type rust` returns nothing (it already does),
   confirming no caller was orphaned by the deletion.
