# Plan: collapse `CheckResult` constructor struct-literal duplication

## Context

`impl CheckResult` in `cli/src/doctor.rs:63-151` defines 8 private constructors
-- `ok`/`warn`/`fail`/`skip` and their subject-tagged `*_for` variants -- each of
which restates the same 4-field struct literal `Self { name, status, message,
subject }`. A `/ultrareview` finding flagged this as constructor sprawl and
proposed collapsing to two public constructors (`new`, `new_for`) that take
`CheckStatus` explicitly at every call site.

Investigation showed the finding's headline facts were wrong (it claimed
"twelve constructors, several unused"; there are exactly **8**, **all used**,
across **89 call sites**, all within `doctor.rs`). The finding's proposed fix is
also the wrong shape: passing `CheckStatus` explicitly would churn all 89 call
sites and make each one *more* verbose (e.g. `CheckResult::skip(name, msg)` ->
`CheckResult::new(name, CheckStatus::Skip, msg)` at 31 sites).

The real -- if minor -- problem is the duplicated struct literal: adding a field
to `CheckResult` today means editing 8 places. This plan fixes exactly that,
with zero call-site churn and zero behavior change. Intended outcome: a new
`CheckResult` field threads through **one** site, the 8 terse named constructors
survive untouched, and all 89 call sites are byte-for-byte unchanged.

## Scope

- **In scope:** the `impl CheckResult` block, `cli/src/doctor.rs:63-151` only.
- **Not changing:** the `CheckResult` struct, the `CheckStatus` enum, any of the
  89 call sites, any check logic, any test. The 8 named constructors keep their
  exact names and signatures (`*_for` keep arg order `name, subject, message`),
  so callers are oblivious to the change.
- **No sibling cleanup:** exploration found no other type with this
  per-variant-builder shape (`StatusTag` in `status_tag.rs` renders via match,
  not per-variant constructors), so there is nothing to unify outward.

## The change

Introduce two private builders that own field initialization, and rewrite the 8
named constructors as one-line delegations. Use struct-update syntax so the
full 4-field initialization appears **exactly once** (in `new`), with `new_for`
layering on the subject -- matching the existing `..Default::default()` idiom used in
`cli/src/ups.rs:348`, `cli/src/ack.rs:1761`, `cli/src/add.rs:4789`, etc.

Replace lines 63-151 with:

```rust
impl CheckResult {
    /// Sole site that initializes all four `CheckResult` fields: every named
    /// constructor and `new_for` delegate here, so a new field is added once.
    fn new(name: impl Into<String>, status: CheckStatus, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            message: message.into(),
            subject: None,
        }
    }

    /// Subject-tagged result (SMART self-test checks); reuses `new`'s field
    /// init and only overrides `subject`.
    fn new_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        status: CheckStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            subject: Some(subject.into()),
            ..Self::new(name, status, message)
        }
    }

    fn ok(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Ok, message)
    }

    fn warn(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Warn, message)
    }

    fn fail(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Fail, message)
    }

    fn skip(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Skip, message)
    }

    fn ok_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Ok, message)
    }

    fn warn_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Warn, message)
    }

    fn fail_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Fail, message)
    }

    fn skip_for(
        name: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new_for(name, subject, CheckStatus::Skip, message)
    }
}
```

### Notes for the implementer

- **`CheckStatus` is `Copy`** (`cli/src/doctor.rs:45`), so passing it by value
  into `new`/`new_for` and again from `new_for` into `new` costs nothing.
- **Formatting:** hand-shape this block to rustfmt conventions -- keep the
  multi-line `fn` bodies (do **not** hand-collapse to `{ ... }` one-liners --
  rustfmt would expand them), and keep the 4-/3-param signatures wrapped as
  shown. There is no Rust fmt-check in CI; per AGENTS.md do **not** run
  `cargo fmt`, `rustfmt`, or any formatter-over-source wrapper -- match the
  shape shown above by hand.
- **Doc comments:** one-line `///` on `new` (and `new_for`) capturing the
  single-source-of-truth intent, matching the documented-shared-helper pattern
  in `doctor.rs` (e.g. `load_membership_or_check_result` at line ~515). The 8
  named wrappers stay bare -- their purpose is evident from name + delegation,
  consistent with the existing (undocumented) constructors.

## Why this shape, not the finding's

| | Finding's proposal | This pivot |
|---|---|---|
| Call sites touched | all 89 (more verbose) | 0 |
| Full field-initialization sites | 2 (`new`, `new_for`) | **1** (`new`) |
| Named builders | deleted | kept |
| Behavior change | none | none |
| Diff size | large (89 sites) | one impl block |

Keeping the named builders preserves call-site readability
(`CheckResult::skip(NAME, msg)` reads better than
`CheckResult::new(NAME, CheckStatus::Skip, msg)`), and the `*_for` call sites
(`doctor.rs:1055-1156`) select a constructor per match-arm rather than passing a
computed status, so explicit-status constructors would buy nothing there.

## Verification

1. `just test-rust` -- runs `cargo test` (package `braid-cli`). The `doctor.rs`
   test module asserts on returned `CheckResult` fields (e.g. `.status`,
   `.subject` at `doctor.rs:1794-1795`, `1815-1818`, `2302-2305`); these must
   stay green, proving field values are unchanged.
2. `cargo build` (or `cargo clippy`) clean -- confirms the struct-update
   delegation and `Copy` status passing compile without warnings. (`new` takes
   args and is private, so `clippy::new_without_default` does not apply.)
3. `git diff --stat` shows only `cli/src/doctor.rs` changed, and `git diff`
   shows changes confined to the `impl CheckResult` block (no call-site lines in
   the diff) -- the structural guarantee that all 89 callers are untouched.

No VM tests needed: this is a pure-Rust, single-file, behavior-preserving
refactor with no systemd/mount/lock blast radius.
