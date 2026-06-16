# Plan: centralize the keyfile name in the enroll no-overwrite message

## Context

`format_keyfile_already_exists` in `cli/src/enroll_key_file.rs` renders the
`enroll --generate` no-overwrite error. It hardcodes the literal `braid.key`
in the leading clause:

```rust
"braid.key already exists at {}.\n\
 If a prior `--generate` run was interrupted, drop `--generate` and re-run \
 `braid enroll {}` to finish enrolling the existing keyfile.\n\
 Otherwise remove it manually if you want to generate a new one."
```

Every keyfile *path* in the same runtime flow is built from
`braid_cli::luks::KEYFILE_NAME` (the const at `cli/src/luks.rs:28`, value
`"braid.key"`) -- see `cli/src/main.rs:563,677,785`. So the `{}` in this
message (which is `key_file_path.display()`, a path main.rs constructed from
the const) and the literal prefix are coupled but only one side tracks the
const. If `KEYFILE_NAME` ever changed, this single message would render a
self-contradicting line like `braid.key already exists at /mnt/usb/newname.`
-- a confusing, hard-to-spot doc/behavior skew. Behavior is unaffected today
because `KEYFILE_NAME == "braid.key"`.

Outcome: the const becomes the single source of truth for the keyfile name in
*all runtime code*, and the remaining (deliberate, cross-boundary)
duplications of the name are documented at the definition site so a future
rename has one authoritative checklist.

## Scope decision (why this shape)

The name is also written in two places that this plan deliberately does **not**
try to dedupe in code, because doing so would violate braid's "ideal, robust,
*simple*" principle:

- **NixOS module** -- an accepted cross-language duplication; the const's doc
  already records it ("hardcoded to match the NixOS auto-unlock module").
- **clap help doc-comments** -- three of them, each hardcoding the literal:
  `add --enroll` (`AddArgs`, `cli/src/main.rs:323`), `replace --enroll`
  (`ReplaceArgs`, `:363`), and the `enroll` DIR positional
  (`EnrollKeyFileArgs`, `:407`). These are compile-time `///` literals
  describing a *directory* argument (illustrative prose, not a path the tool
  constructs and compares). Interpolating a const into them would require a
  new `const_format` dependency purely for help text -- more machinery, and
  still not project-wide single-source-of-truth because Nix duplicates the
  name regardless. Rejected.

braid's convention for a duplication you cannot dedupe across a
language/macro boundary is to make it explicit at the definition site (as the
const already does for Nix). So we extend that note instead of adding code.

## Changes

### 1. `cli/src/enroll_key_file.rs` -- interpolate the const

- Add `KEYFILE_NAME` to the existing `use crate::luks::{...}` block
  (`cli/src/enroll_key_file.rs:7-10`), alphabetically beside its sibling
  `KEYFILE_SIZE` which is already imported there. No new `use` statement.
- In `format_keyfile_already_exists` (`cli/src/enroll_key_file.rs:647`),
  change the format string's leading literal from `braid.key` to the captured
  identifier `{KEYFILE_NAME}`:

  ```rust
  format!(
      "{KEYFILE_NAME} already exists at {}.\n\
       If a prior `--generate` run was interrupted, drop `--generate` and re-run \
       `braid enroll {}` to finish enrolling the existing keyfile.\n\
       Otherwise remove it manually if you want to generate a new one.",
      key_file_path.display(),
      dir.display()
  )
  ```

  The two positional `{}` still map to `key_file_path.display()` and
  `dir.display()`; inline-captured args do not consume positional slots. This
  file already uses inline capture for in-scope values (e.g. `{underlying}` at
  `:607`, `{dir_display}` at `:678`), and the same capture resolves an
  in-scope `const`. Output is byte-identical today.

This single helper feeds both call sites -- the plan-time check
(`validate_key_file_path`, `:626`) and the mutation-boundary failure in
`EnrollPlan::execute` (`:576`) -- so the one edit fixes both rendered messages.

### 2. `cli/src/luks.rs` -- make the const's doc the duplication registry

Extend the existing doc comment on `KEYFILE_NAME` (`cli/src/luks.rs:27`) so it
lists every place the literal is re-hardcoded, not just Nix:

```rust
/// Canonical keyfile filename, hardcoded to match the NixOS auto-unlock
/// module. The name is also written as a literal in three clap help strings
/// -- `add --enroll` (`AddArgs`), `replace --enroll` (`ReplaceArgs`), and the
/// `enroll` DIR positional (`EnrollKeyFileArgs`) in main.rs -- which cannot
/// interpolate this const; keep them in sync on any rename.
pub const KEYFILE_NAME: &str = "braid.key";
```

(Wording is illustrative; match surrounding doc-comment style.)

## Tests

No test changes. The existing assertions already cover the rendered message
and actively guard it against skew:

- `generate_rejects_existing_keyfile_after_mountpoint_check`
  (`cli/src/enroll_key_file.rs:1545`) -- asserts
  `.contains("braid.key already exists")`.
- `execute_generate_existing_keyfile_at_boundary_reports_friendly_error`
  (`cli/src/enroll_key_file.rs:3432`) -- asserts
  `msg.contains("braid.key already exists")`.
- `tests/cli/braid-enroll-generate.py` ("Test 3: --generate refuses
  overwrite", ~`:117`) -- a NixOS VM test asserting the *live* CLI output
  contains `"braid.key already exists"`; end-to-end proof the rendered message
  is unchanged (separate VM lane, not run by `just test-rust`).

All three pass unchanged (byte-identical output). They are behavioral and
structure-insensitive: a future `KEYFILE_NAME` rename would flip the rendered
text and fail these, flagging the runtime path -- so no new structure-sensitive
"uses the const" test is warranted. The clap help has no such guard, which is
exactly why it gets the doc-note in change #2 instead.

## Verification

1. `cargo fmt` then `just clippy` (runs `cargo clippy --manifest-path
   cli/Cargo.toml --tests`; the package is `braid-cli`, so `-p braid_cli` does
   not resolve) -- confirms the `{KEYFILE_NAME}` capture compiles clean.
2. Targeted tests:
   `cargo test generate_rejects_existing_keyfile_after_mountpoint_check`
   and
   `cargo test execute_generate_existing_keyfile_at_boundary_reports_friendly_error`
   -- both green.
3. Full Rust suite: `just test-rust`.
4. Source-level confirmation (scoped, not repo-wide): `rg "braid.key already
   exists" cli/src/enroll_key_file.rs` should now match only the two test
   assertions (~`:1571`, ~`:3497`) and the adjacent comment (~`:3495`) -- the
   former format-string match (was ~`:650`) is gone, since the source now
   derives the prefix from the const. A repo-wide `git grep` is the wrong
   check: it also matches historical `plans/impl/*` files and the VM test, so
   "only two matches" would never hold. Byte-identical *output* is proven by
   the unchanged assertions in the Tests section, not by this grep.
5. ASCII-output lint: `scripts/docs/check-output-ascii.py` (the edited string
   stays ASCII).
