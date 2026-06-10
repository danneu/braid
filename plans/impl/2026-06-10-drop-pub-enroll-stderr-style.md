# Drop `pub` from enroll's `STDERR_STYLE` const

## Context

Commit `4c850a36` ("refactor(cli): inline dead per-disk stderr style consts")
deleted six dead `STDERR_STYLE` consts from the other command Plan types and
inlined their literals. `EnrollPlan::STDERR_STYLE = PerDiskStyle::Plain` was
deliberately kept because it's observable and tested: it preserves the legacy
`skip: <name> not present` stderr wording byte-for-byte across the dry-run
migration.

That refactor's plan flagged an optional, separable tidy it left out to keep the
diff minimal: the const is declared `pub`, but nothing outside
`enroll_key_file.rs` references it, so the `pub` is unjustified API surface. This
plan completes that tidy — narrowing visibility to match actual usage. No
behavior change.

## Change

In `cli/src/enroll_key_file.rs`, inside `impl EnrollPlan` (the const definition,
currently line 512):

```rust
-    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Plain;
+    const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Plain;
```

Leave the `///` doc comment, the value (`PerDiskStyle::Plain`), and every
reference site unchanged. This is the only edit.

## Why it's safe

`grep -rn STDERR_STYLE cli/src/` returns matches only in
`enroll_key_file.rs` (verified). The five lines:

| Line | Site | Reaches a private const? |
|------|------|--------------------------|
| 495  | struct/impl docstring (`/// ... \`STDERR_STYLE\``) | n/a — comment text |
| 512  | the definition | — |
| 530  | `execute()` -> `Self::STDERR_STYLE` | yes, same `impl` block |
| 826  | `cmd_enroll` error arm -> `EnrollPlan::STDERR_STYLE` | yes, same root module |
| 1336 | one test -> `EnrollPlan::STDERR_STYLE` | yes, see below |

Visibility analysis (confirmed by reading the file structure):

- `impl EnrollPlan` is at the **root module** of `enroll_key_file.rs` (column 0).
- Lines 530 and 826 live in that same root module, so a private associated
  const is fully in scope.
- Line 1336 is inside `#[cfg(test)] mod tests` (opens at line 839-840), a
  **child** module of the root. In Rust, a child module can read private items
  of its ancestor modules, so the `#[cfg(test)]` test still compiles against a
  non-`pub` const.

No external crate or sibling module reads `STDERR_STYLE`, so dropping `pub`
removes surface without affecting any caller. No new test is warranted — there is
no behavior change, and the existing line-1336 test already pins the observable
`skip: disk1 not present` stderr wording that motivated keeping the const.

## Verification

Run from the repo root, in order:

1. `cargo build -p braid-cli` — catches the visibility / compile (the non-test
   usages at lines 530 and 826).
2. `just clippy` — must be clean (no new dead-code or visibility lint).
3. `just test-rust` — the gate that compiles the `#[cfg(test)] mod tests` module,
   confirming the test at line 1336 still reads the now-private const.

## Commit

Stage only `cli/src/enroll_key_file.rs` and commit:

```
refactor(cli): drop pub from enroll's stderr-style const

Nothing outside enroll_key_file.rs reads EnrollPlan::STDERR_STYLE.
```

(First line lowercase per Conventional Commits; one-line body noting nothing
outside the module reads it.)
