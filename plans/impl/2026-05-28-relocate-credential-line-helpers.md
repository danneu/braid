# Relocate credential-line helpers out of `status_tag.rs`

## Context

`status_tag.rs` is the generic, cross-command event-log tag renderer
(`StatusTag`, `render_status_tag`, `status_line`, `emit_status`, color
helpers, `testing`). It is imported by ~14 command modules. Three items in
it -- `CredentialKind`, `credential_wait_line`, `credential_ok_line` -- are
not generic rendering primitives: they are LUKS credential-verification UI
whose **only** consumer is `credential_verify.rs`. Their presence in the
shared renderer widens its apparent surface and couples credential-wording
changes to a file that otherwise owns no command-specific messaging.

Verified facts (read-only investigation):
- No references to the three symbols anywhere outside
  `cli/src/credential_verify.rs` (production + tests). Not in `cli/tests/`,
  not in any other `cli/src/*.rs`.
- No `pub use` re-exports of them anywhere in `cli/src/`.
- The helpers are pure thin wrappers over `status_line(...)` and add zero
  color logic of their own.

Outcome: move all three (and their test) next to their sole consumer,
make them module-private, and leave `status_tag.rs` as the generic
tag/line renderer. Pure relocation -- no behavior or output change.

## The move

Move out of `cli/src/status_tag.rs` and into `cli/src/credential_verify.rs`:

1. `CredentialKind` enum + `impl CredentialKind { fn label(self) }`
   (`status_tag.rs:21-34`).
2. `credential_wait_line` (`status_tag.rs:76-82`).
3. `credential_ok_line` (`status_tag.rs:84-90`).

Apply these refinements during the move:

- **Drop `pub`.** All three currently `pub`; nothing outside
  `credential_verify.rs` consumes them, so make them module-private
  (`enum CredentialKind`, `fn credential_wait_line`, `fn credential_ok_line`).
  `label()` is already private. No dead-code warnings result: all three are
  used in `credential_verify.rs` **production** code -- `CredentialKind` at
  lines 42-43 and 100, `credential_wait_line` at 47 and 99, `credential_ok_line`
  at 59 -- not just in tests.
- **Add a one-line doc comment to `CredentialKind`.** It is currently
  undocumented. Per the repo doc-comment rule, justify why it exists as a
  distinct boundary -- e.g. "Cheap `Copy` display discriminant for
  credential-verification rows; deliberately separate from `Credential<'a>`,
  which borrows the live secret." This directly dissolves the
  reviewer-confusion class the finding flagged. The two line helpers are
  self-evident wrappers -- no doc comment needed.

### Destination placement in `credential_verify.rs`

- Put `CredentialKind` + its impl immediately after the `Credential<'a>`
  enum (currently ends line 19) -- it is the display discriminant chosen
  from `Credential` in `verify_credential_for_targets`.
- Put `credential_wait_line` / `credential_ok_line` just before
  `verify_credential_for_targets` (their first caller, currently line 34).

### Import line update in `credential_verify.rs`

Current (`credential_verify.rs:4-6`):
```rust
use crate::status_tag::{
    CredentialKind, StatusTag, credential_ok_line, credential_wait_line, status_line,
};
```
After (the three names are now local; `StatusTag` + `status_line` still
come from `status_tag`):
```rust
use crate::status_tag::{StatusTag, status_line};
```
The test module reaches the relocated items through its existing
`use super::*;` (line 121) -- no test-side import edits needed.

## What stays put

- **`strip_ansi`** stays in `status_tag.rs`'s test module
  (`status_tag.rs:182`). It is still used by two tests that remain there
  (`status_line_prefix_is_seven_visible_columns`,
  `colored_status_tags_strip_to_plain_tags`). It is **not** moved or
  duplicated.
- The `status_tag.rs` module-level doc comment (lines 3-11) already
  describes only `StatusTag`/`status_line` -- no edit needed.

## Test handling (Option B: drop the redundant colored assertions)

Move `credential_wait_line_formats_known_credentials`
(`status_tag.rs:299-336`) into `credential_verify.rs`'s `#[cfg(test)] mod
tests`. Keep the four plain-mode wording assertions verbatim:

```rust
credential_wait_line(CredentialKind::Passphrase, false, "disk1") == "[wait] passphrase: checking against disk1...\n"
credential_wait_line(CredentialKind::KeyFile,    false, "disk1") == "[wait] keyfile: checking against disk1...\n"
credential_ok_line(CredentialKind::Passphrase,   false, "disk1") == "[ok]   passphrase: accepted by disk1\n"
credential_ok_line(CredentialKind::KeyFile,      false, "disk1") == "[ok]   keyfile: accepted by disk1\n"
```

**Drop** the two `strip_ansi(...)` colored-mode assertions
(`status_tag.rs:324-335`). They are redundant: the helpers pass
`color_enabled` straight through to `status_line`, and the
"colored strips back to plain" invariant is already pinned independently in
`status_tag.rs` by `colored_status_tags_strip_to_plain_tags` and
`status_line_prefix_is_seven_visible_columns`. Removing them means the moved
test needs no `strip_ansi`, so nothing is duplicated.

Update the test's preamble accordingly: the current `Scenario` line says
"render in plain and colored modes" -- change to plain mode only. Preserve
the three-section preamble form (Intent / Why it exists / Scenario) per the
test convention.

## Considered and rejected

- **Eliminate `CredentialKind` entirely** by hanging `label()` off
  `Credential<'a>` (which is already `Copy`). Rejected: it couples a
  display-label concern to the enum that carries the live secret. Keeping a
  small, intention-revealing discriminant separate from the secret-bearing
  type is the cleaner boundary.
- **Hoist `strip_ansi` to a shared test-util module.** Rejected: a new
  cross-module test helper is more surface than the duplication it would
  save for a 16-line function -- and Option B removes the need entirely.

## Files touched

- `cli/src/status_tag.rs` -- remove the three items (21-34, 76-90) and the
  one test (299-336); keep everything else, including `strip_ansi`.
- `cli/src/credential_verify.rs` -- add the three items (private, with a doc
  comment on `CredentialKind`); shrink the `status_tag` import; add the moved
  test (plain-mode assertions only).

## Verification

- `just test-rust` -- runs `cargo test` for `braid-cli`. The moved test and
  the two `strip_ansi` tests left behind in `status_tag.rs` must pass; the
  existing `credential_verify.rs` tests (which already exercise
  `credential_wait_line`/`credential_ok_line`/`CredentialKind` via
  `expected_wait_ok_pairs`, `with_case`, the probe tests) must stay green.
- `cargo check` (or the build leg of `just test-rust`) -- confirm no
  unused-import warning on the trimmed `status_tag` import and no dead-code
  warning on the now-private items.
- **No VM tests required.** This is a pure Rust-internal relocation with no
  output, parser, systemd, lifecycle, or mount/lock impact; per the repo
  test-scope guidance a small localized change warrants only the focused
  Rust run above.

## Implementation notes

- Converted the moved test's preamble from the `/* ... */` block form it
  used in `status_tag.rs` to the `//` line-comment form. The destination
  file's other preambled tests (the `probe_*` tests) use `//`, and the
  AGENTS.md test convention specifies a `//` line-comment preamble, so the
  moved test now matches its new neighbors. Content is unchanged except the
  Scenario line (now "plain mode" only) per the plan.
