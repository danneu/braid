# Pin keyfile dry-run rendering and clarify `compile_open_steps`'s role

## Context

`mount::compile_open_steps` (`cli/src/mount.rs:397`) takes a
`key_file: Option<&Path>` and emits one of two `CmdRequest` variants
(`CryptsetupLuksOpen` vs `CryptsetupLuksOpenKeyFile`). The two variants
render genuinely different cryptsetup invocations in the dry-run
preview (`--key-file=-` stdin vs `--key-file <path> --keyfile-size
4096`). A code-review finding recently proposed deleting the branch on
the (incorrect) premise that the keyfile arm only changes a description
string. Two concrete weaknesses surfaced while verifying that finding:

1. **No test pins the keyfile arm.** The only `plan_unlock` dry-run
   unit test (`plan_unlock_dry_run_render_2_closed_disks` at
   `cli/src/unlock.rs:1152`) sets `key_file: None`. If someone deletes
   the keyfile branch in `compile_open_steps`, no Rust test fails.
   Operators running `braid unlock --key-file <path> --dry-run` would
   silently see a misleading argv -- the exact case auto-unlock users
   are most likely to hit dry-run for.
2. **`compile_open_steps`'s preview-only role is non-obvious.** The
   confusion in the finding came from "real-run open also constructs
   `CmdRequest::CryptsetupLuksOpen`, so why does this function take
   `key_file`?" The answer -- real-run open goes through
   `luks::ensure_luks_open` / `ensure_luks_open_with_key_file`, each of
   which builds its own `CmdRequest` -- isn't visible at the call site
   or in the doc comment.

This plan adds the missing regression guard and clarifies the doc
comment. Both fit cleanly in one commit.

Out of scope: the finding's proposed API split into
`compile_open_steps_passphrase` / `_key_file`. It's cosmetic (the
branch just moves to the unlock caller) and not worth churn.

## Approach

### 1. New test in `cli/src/unlock.rs`

Add a sibling to `plan_unlock_dry_run_render_2_closed_disks` (place it
immediately after, lines ~1152-1254). Copy its exact setup verbatim --
same `Config`, `PoolMembership`, `MockFs`, and `MockRunner` builder
chain (`with_luks_dump_text_luks2_for` + `with_mappers_closed`) -- and
change only the `UnlockParams.key_file` field to
`Some(Path::new("/run/keys/braid.key"))`.

Assertions (behavioral, structure-insensitive -- `.contains()` not
byte-equality, but tight enough to lock in the keyfile-open variant
specifically rather than any command that happens to carry
`--keyfile-size`):

- `rendered.contains("cryptsetup open --type luks --key-file
  /run/keys/braid.key --keyfile-size 4096")` -- pins the full
  distinguishing prefix of `CryptsetupLuksOpenKeyFile`'s argv
  (`cli/src/cmd.rs:725-744`). The substring `open --type luks`
  excludes `CryptsetupTestKeyFile` (which uses `open
  --test-passphrase`, `cli/src/cmd.rs:745-759`) and the bare
  `--key-file <path>` form (no `=-`) excludes
  `CryptsetupLuksOpen`'s passphrase-via-stdin shape. Path renders
  unquoted: `shell_words::join` only quotes args containing
  whitespace or shell metachars (`cli/src/cmd.rs:255-259`);
  `/run/keys/braid.key` is bare.
- `assert!(!rendered.contains("--key-file=-"))` -- locks in "not the
  passphrase-via-stdin variant." Without this, a regression that
  emitted both step variants for the same disk would still pass the
  positive assertion above.
- Keep the existing test's `LUKS open <by_id>` description-line
  assertion so the description-vs-argv distinction is visible
  side-by-side with the original test.

Test preamble: per `docs/testing.md` "Preamble: literal `//`
line-comment form", the preamble is a contiguous block of `//` line
comments directly above `#[test]` (NOT a `/* */` block, even though
some adjacent tests in `cli/src/unlock.rs` still use the older block
form). Three contiguous lines:

```rust
// Intent: with `--key-file <path>`, the dry-run preview emits the
//   keyfile cryptsetup invocation (`cryptsetup open --type luks
//   --key-file <path> --keyfile-size 4096`), not the
//   passphrase-via-stdin form.
// Why it exists: `compile_open_steps` is the only place the
//   passphrase-vs-keyfile dry-run branch is rendered, and today's
//   only `plan_unlock` dry-run test exercises the `None` arm.
//   Without this test, deleting the keyfile branch silently
//   regresses the preview fidelity that auto-unlock operators rely
//   on when sanity-checking `braid unlock --key-file <path>
//   --dry-run`.
// Scenario: auto-unlock user runs `braid unlock --key-file
//   /run/keys/braid.key --dry-run` against a 2-disk closed pool.
#[test]
fn plan_unlock_dry_run_render_2_closed_disks_with_key_file() { ... }
```

No new helpers. The setup is small enough to duplicate; factoring a
shared helper for two callers is premature.

### 2. Doc comment on `compile_open_steps` in `cli/src/mount.rs:396`

Replace the current one-line doc:

```rust
/// Compile dry-run steps from a validated OpenPlan.
```

With a 3-line version that captures the preview-only invariant and
the call-site coupling -- per AGENTS.md "Doc Comments" guidance
(intent/invariant/coupling, one to three lines, do not duplicate
what the test pins):

```rust
/// Compile output-only dry-run preview steps from a validated `OpenPlan`.
/// Real execution consumes the `OpenPlan` directly and constructs LUKS
/// requests through `luks::ensure_luks_open` / `ensure_luks_open_with_key_file`.
```

## Critical files

- `cli/src/unlock.rs` -- add new test after line 1254 (end of
  `plan_unlock_dry_run_render_2_closed_disks`).
- `cli/src/mount.rs:396-397` -- replace doc comment on
  `compile_open_steps`.

Reference (read-only, no edits):

- `cli/src/cmd.rs:445-459` and `:725-744` -- the two `to_argv` arms
  whose divergence the new test pins.
- `cli/src/cmd.rs:272-280` -- `Step::render_dry_run`, the renderer the
  test exercises.
- `cli/src/luks.rs:745` and `:790` -- `ensure_luks_open` /
  `ensure_luks_open_with_key_file`, the real-run path the doc comment
  references.

## Verification

- `just test-rust` -- new test must pass on current code (it's a
  regression guard, not a fix). Sanity-check: temporarily collapse the
  `if let Some(kf)` branch in `compile_open_steps` to always emit
  `CryptsetupLuksOpen`, rerun `just test-rust`, confirm the new test
  fails with a missing-`--keyfile-size` assertion. Revert before
  committing.
- No VM tests (`just test-vm`) needed -- this is unit-level preview
  rendering with a `MockRunner`, no NixOS surface touched.
- No fixture refresh needed -- no parser-critical tool versions
  involved.
