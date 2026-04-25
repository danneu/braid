# Plan: confirm LUKS passphrase when `braid add` formats without a verify target

## Context

When `braid add` runs `luks_format` on a new disk (`cli/src/add.rs:548`),
the CLI prompts for a LUKS passphrase exactly once
(`cli/src/luks.rs:89-95`, `prompt_passphrase_tty`). There is no
confirmation re-prompt. A typo on the fresh-format path silently becomes
the canonical passphrase for the resulting LUKS header -- unrecoverable
without an external key backup.

Today, the only typo protection in `cmd_add` is the verify block at
`add.rs:422-439`, which calls `verify_passphrase` against
`pool.devices.first()` when a live pool member is present. On any path
where that guard is skipped -- a fresh bootstrap, or a fresh-disk add to
a pool that isn't currently assembled -- the user gets no protection.

Scope of this change: **add a second prompt on the TTY whenever
`cmd_add` is about to `luks_format` and there is no live verify target
available.** `replace`, `enroll-key-file`, `unlock`, and `recover` are
unchanged -- each either verifies against a live keyslot or fails safely
on a typo.

## Design

### Gate

```rust
let confirm_new = any_needs_format && pool.devices.is_empty();
```

Semantically equivalent to `pool.devices.first().is_none()` at
`add.rs:422` (clippy prefers `is_empty()` over `first().is_none()`).
`any_needs_format` is computed at `add.rs:352`; reuse it. The full
2-axis matrix of `(any_needs_format, live_target_present)`:

| any_needs_format | live_target | confirm_new | Notes |
| --- | --- | --- | --- |
| true  | false | **true**  | Bootstrap, or fresh disk into unassembled pool. No safety net. |
| true  | true  | false | Fresh disk into live pool. `verify_passphrase` catches typos before format. |
| false | *     | false | No format happening; typos are non-catastrophic (later identity/open handling aborts). |

Avoid proxy signals like `pool_membership.disks.is_empty()` -- they
false-positive on recovery-style adds and on PresentLuks-only adds on
virgin installs.

### API shape: unified seam for both `cmd_add` branches

The gate is a behavior regression on `cmd_add`. Tests must bind to
`cmd_add` for all four matrix cells. That means `cmd_add`'s passphrase
read must go through a single test seam regardless of branch.

Add a unified entry point `read_passphrase_with` that carries
`confirm_new` and a `PassphraseReader` trait object. The existing
`read_passphrase` becomes a thin wrapper so other callers are
undisturbed.

Also add an internal stdin seam paralleling `confirm_yes_from`
(`cli/src/confirm.rs:101`) so stdin-path tests don't have to swap
process stdin.

`cli/src/luks.rs`:

```rust
pub trait PassphraseReader {
    /// Read a passphrase from the terminal with the given prompt label,
    /// suppressing echo. Returns validated (non-empty, no embedded
    /// newlines) passphrase.
    fn read_tty(&self, label: &str) -> Result<String, LuksError>;
}

pub struct RealTty;

impl PassphraseReader for RealTty {
    fn read_tty(&self, label: &str) -> Result<String, LuksError> {
        eprint!("{label}");
        let raw = rpassword::read_password().map_err(...)?;
        validate_passphrase(&raw, "terminal")
    }
}

/// Test-only scripted reader. Module scope so add.rs tests can import
/// it via `use crate::luks::ScriptedPassphraseReader;`. Returns from a
/// FIFO queue and errors (not panics) on exhaustion so over-consumption
/// surfaces as a test failure.
#[cfg(test)]
pub(crate) struct ScriptedPassphraseReader {
    queue: std::cell::RefCell<std::collections::VecDeque<String>>,
}

#[cfg(test)]
impl ScriptedPassphraseReader {
    pub(crate) fn new<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    { /* ... */ }
    pub(crate) fn remaining(&self) -> usize { /* ... */ }
}

#[cfg(test)]
impl PassphraseReader for ScriptedPassphraseReader {
    fn read_tty(&self, _label: &str) -> Result<String, LuksError> {
        match self.queue.borrow_mut().pop_front() {
            Some(s) => Ok(s),
            None => Err(LuksError::Validation(
                "ScriptedPassphraseReader: queue exhausted".into(),
            )),
        }
    }
}

/// Thin wrapper -- locks process stdin, delegates.
pub fn read_passphrase_with(
    passphrase_file: Option<&Path>,
    passphrase_stdin: bool,
    confirm_new: bool,
    tty: &dyn PassphraseReader,
) -> Result<String, LuksError> {
    let mut stdin = std::io::stdin().lock();
    read_passphrase_with_readers(
        passphrase_file, passphrase_stdin, confirm_new, &mut stdin, tty,
    )
}

/// Full form. File/stdin paths ignore `confirm_new` and `tty`.
/// TTY path: prompts once, or twice + byte-equality check if
/// `confirm_new` is true. Private so callers must go through
/// `read_passphrase_with` (real stdin) or direct tests (Cursor stdin).
fn read_passphrase_with_readers(
    passphrase_file: Option<&Path>,
    passphrase_stdin: bool,
    confirm_new: bool,
    stdin: &mut dyn BufRead,
    tty: &dyn PassphraseReader,
) -> Result<String, LuksError> {
    if let Some(path) = passphrase_file { /* file branch */ }
    if passphrase_stdin {
        return read_passphrase_stdin_from(stdin);
    }
    let first = tty.read_tty("LUKS passphrase: ")?;
    if !confirm_new {
        return Ok(first);
    }
    let second = tty.read_tty("Confirm LUKS passphrase: ")?;
    check_passphrase_match(first, second)
}

/// Existing API, signature preserved for callers that don't need the
/// seam (replace, enroll, mount). Delegates with
/// `confirm_new = false` and the production TTY reader.
pub fn read_passphrase(
    passphrase_file: Option<&Path>,
    passphrase_stdin: bool,
) -> Result<String, LuksError> {
    read_passphrase_with(passphrase_file, passphrase_stdin, false, &RealTty)
}

/// Testable stdin reader (parallels `confirm_yes_from`). Takes a
/// trait-object `&mut dyn BufRead` so both `StdinLock` (production) and
/// `Cursor` (tests) flow through unchanged. The production caller locks
/// stdin and passes it in; tests pass `Cursor`.
fn read_passphrase_stdin_from(r: &mut dyn BufRead) -> Result<String, LuksError> {
    let mut buf = String::new();
    r.read_line(&mut buf)?;
    validate_passphrase(&buf, "stdin")
}

fn check_passphrase_match(first: String, second: String) -> Result<String, LuksError> {
    if first == second {
        Ok(first)
    } else {
        Err(LuksError::Validation(
            "passphrases do not match -- aborting".to_owned(),
        ))
    }
}
```

Refactor the existing private `prompt_passphrase_tty` (and any TTY-reach
in `read_passphrase`) to delegate through `RealTty.read_tty`, so
there is one rpassword implementation.

### Seam wiring in `AddParams`

Same pattern as the existing
`sleep_inhibitor: &'a dyn AcquireSleepInhibitor`:

```rust
pub struct AddParams<'a> {
    ...
    pub passphrase_reader: &'a dyn PassphraseReader,
}
```

Production (`cli/src/main.rs`) passes `&RealTty`. Tests pass a
scripted reader.

In `cmd_add` -- single call site for both branches:

```rust
let confirm_new = any_needs_format && pool.devices.is_empty();
let passphrase = read_passphrase_with(
    params.passphrase_file,
    params.passphrase_stdin,
    confirm_new,
    params.passphrase_reader,
)?;
```

Delete the direct `read_passphrase` call at `add.rs:419`.

### UX

Single-shot. Mismatch returns
`LuksError::Validation("passphrases do not match -- aborting")`; user
re-runs `braid add`. No in-loop retry -- mirrors how the CLI treats
other validation errors.

## Files

- `cli/src/luks.rs` -- add `PassphraseReader` trait, `RealTty`,
  module-scope `#[cfg(test)] pub(crate) ScriptedPassphraseReader`
  (shared with `add.rs` tests), public `read_passphrase_with`, private
  `read_passphrase_with_readers`, `read_passphrase_stdin_from`,
  `check_passphrase_match`; refactor `prompt_passphrase_tty` to
  delegate through `RealTty`; `read_passphrase` becomes a thin
  wrapper. Also `use std::io::BufRead;` at the top for the `&mut dyn
  BufRead` signatures.
- `cli/src/add.rs` -- add `passphrase_reader: &'a dyn PassphraseReader`
  field to `AddParams` (import `PassphraseReader` in the top-level
  `use crate::luks::{...}`); replace direct `read_passphrase` call
  with `read_passphrase_with` gated on `confirm_new`; update every
  existing test harness that constructs `AddParams` to pass
  `passphrase_reader: &RealTty` (those tests use
  `passphrase_file: Some(...)`, so the TTY reader is never consulted
  -- safe placeholder). In the `#[cfg(test)] mod tests` block, add
  `use crate::luks::{RealTty, ScriptedPassphraseReader};` so the
  production reader is only imported under cfg(test) (it would
  otherwise trigger `unused_import` since non-test code only needs
  `PassphraseReader`).
- `cli/src/main.rs` -- construct `&braid_cli::luks::RealTty` and
  pass it as `passphrase_reader` into `AddParams` for the
  `Commands::Add` dispatch.
- No changes to `replace.rs`, `enroll_key_file.rs`, `mount.rs`.

## Tests

### Pure helper tests (in `cli/src/luks.rs`)

1. `check_passphrase_match_ok_on_equal` -- `"secret" == "secret"` ->
   Ok returns `"secret"`.
2. `check_passphrase_match_err_on_differ` -- `"abc" != "xyz"` ->
   `Err(LuksError::Validation(msg))` with `msg.contains("do not match")`.
3. `check_passphrase_match_case_sensitive` -- `"ABC" != "abc"` -> Err.
4. `check_passphrase_match_trailing_whitespace_sensitive` --
   `"abc" != "abc "` -> Err.

### Stdin seam tests (in `cli/src/luks.rs`)

Modeled after `confirm_yes_from` tests in `cli/src/confirm.rs:177-200`.

5. `read_passphrase_stdin_from_ok` -- `Cursor::new(b"secret\n")` ->
   `Ok("secret")`.
6. `read_passphrase_stdin_from_empty_rejected` -- `Cursor::new(b"\n")`
   -> `Err(Validation)`.
7. `read_passphrase_stdin_from_strips_crlf` --
   `Cursor::new(b"secret\r\n")` -> `Ok("secret")`.

### Branch-selection tests (in `cli/src/luks.rs`)

All call `read_passphrase_with_readers` directly so branch selection is
pinned without process-stdin manipulation. Define
`ScriptedPassphraseReader` with a `RefCell<VecDeque<String>>` queue at
**`cli/src/luks.rs` module scope under `#[cfg(test)] pub(crate)`** (not
inside the `tests` submodule) so it can be imported by `add.rs` tests
too (`use crate::luks::ScriptedPassphraseReader;`). Tests assert
remaining queue length to detect over-consumption.

8. `read_passphrase_with_readers_tty_no_confirm_single_read` -- TTY
   branch, `confirm_new: false`, queue `["pw", SENTINEL]`, stdin empty
   Cursor. Assert: returns `"pw"`; queue has 1 entry remaining.
9. `read_passphrase_with_readers_tty_confirm_consumes_two` -- TTY
   branch, `confirm_new: true`, queue `["pw", "pw"]`, stdin empty
   Cursor. Assert: returns `"pw"`; queue empty.
10. `read_passphrase_with_readers_tty_confirm_mismatch_err` -- TTY
    branch, `confirm_new: true`, queue `["pw", "typo"]`, stdin empty
    Cursor. Assert: `Err(Validation(msg))` with
    `msg.contains("do not match")`; queue empty.
11. `read_passphrase_with_readers_file_short_circuits_stdin_and_tty` --
    file branch (tempfile), `confirm_new: true`, queue `[SENTINEL]`,
    stdin `Cursor::new(b"STDIN_SHOULD_NOT_BE_READ\n")`. Assert: returns
    file contents; queue unchanged (SENTINEL still present); stdin
    cursor position == 0 (unread).
12. `read_passphrase_with_readers_stdin_short_circuits_tty` --
    `passphrase_stdin: true`, `confirm_new: true`, queue `[SENTINEL]`,
    stdin `Cursor::new(b"from-stdin\n")`. Assert: returns
    `"from-stdin"`; queue unchanged (SENTINEL still present). **This
    pins the stdin-vs-TTY branch selection that the planned regression
    chain otherwise wouldn't catch.**

### Cmd-level regression tests (in `cli/src/add.rs`)

Model after the existing `cmd_add` test at `add.rs:1864+` (mocked
runner + tempdir state paths). The scripted reader is imported from
`crate::luks::ScriptedPassphraseReader` (defined at module scope in
luks.rs so both test files can use it).

Write a file-local `AddRecordingRunner` in the `add.rs` test module
that logs every `run`/`run_with_stdin` call (use
`Arc<Mutex<Vec<CmdRequest>>>` for the call log and
`Arc<Mutex<Vec<(CmdRequest, Vec<u8>)>>>` for stdin). Stub only what's
needed to reach `cryptsetup luksFormat`, then force
`CryptsetupLuksHeaderBackup` to return exit 1 so `cmd_add` aborts
deterministically after format runs -- avoids mocking the full
mkfs/mount/probe-pool chain.

For bootstrap tests (13-14): use a fresh `confirm_test_setup` helper
(tempdir + config only, no membership seed). For tests 15-16 that
need pre-seeded membership, reuse `add_test_setup`. Tests 13-15 pair
with `AddRecordingRunner::new(false)` (unmounted); test 16 uses
`AddRecordingRunner::new(true)` and returns exit 0 for
`CryptsetupTestPassphrase` so `verify_passphrase` authenticates.

Cover every fresh-format cell (`any_needs_format = true`). The
`any_needs_format = false` rows short-circuit the gate before it
evaluates `pool.devices.first()`, and their prompt behavior is not part
of the contract this change is pinning; helper-level tests 8 and 11
already cover `confirm_new = false` behavior.

13. `cmd_add_bootstrap_aborts_on_passphrase_mismatch`
    -- `(any_needs_format=true, live_target=false)`, membership empty,
    queue `["typo-one", "typo-two"]`. Assert:
    - `Err(AddError::Luks(LuksError::Validation(msg)))` with
      `msg.contains("do not match")`.
    - Recorded runner has **zero** `CryptsetupLuksFormat` invocations.
      Load-bearing: reverting the gate or dropping the mismatch check
      flips this.
14. `cmd_add_bootstrap_proceeds_on_passphrase_match`
    -- same shape, queue `["ok", "ok"]`. Assert: recorded
    `CryptsetupLuksFormat` invocation with stdin payload `"ok"`; queue
    empty (both reads consumed).
15. `cmd_add_existing_membership_no_live_target_confirms`
    -- `(any_needs_format=true, live_target=false)` but with
    `pool_membership.disks` non-empty (previously-added disk recorded;
    pool not currently assembled). Queue `["pw", "pw", SENTINEL]`.
    Assert: `CryptsetupLuksFormat` runs with `"pw"`; two reads
    consumed (SENTINEL remains). This pins the gate's inclusion of
    "fresh disk into unassembled pool" that the rejected
    `membership.is_empty()` gate would have missed.
16. `cmd_add_live_pool_fresh_add_single_prompt`
    -- `(any_needs_format=true, live_target=true)`. Live mounted pool
    with an existing member; new fresh PresentNotLuks disk being added.
    Queue `["pw", SENTINEL]`. `verify_passphrase` mocked to return
    `Authenticated`. Assert:
    - One read consumed (SENTINEL remains).
    - Recorded runner has the `CryptsetupTestPassphrase`
      (verify_passphrase) call followed by the `CryptsetupLuksFormat`
      call. This pins the gate's exclusion of the live-pool shape that
      a `confirm_new = any_needs_format` regression would over-prompt.
Regression coverage per Plan Review Protocol:
- **Primary behavior (typo in fresh format -> no format):** test 13.
- **Happy path (match -> format):** test 14.
- **Gate includes unassembled-pool fresh format:** test 15.
- **Gate excludes live-pool fresh format:** test 16.
- **Helper correctness:** tests 1-4.
- **Stdin-line reader correctness:** tests 5-7.
- **Branch selection (TTY prompts, file/stdin short-circuit):** tests 8-12.

No VM test. The rpassword/TTY boundary stays untestable; the scripted
`PassphraseReader` + `Cursor`-backed stdin helper give full cmd-level
coverage without a PTY.

## Verification

1. `just test-rust` -- new and existing unit tests pass.
2. `just test-vm` -- existing VM tests pass; `AddParams` signature
   change fails-loudly at compile time if any callsite missed wiring.
3. Manual TTY verification in a VM running `braid add`:
   - Fresh pool, matching passphrases typed -> pool created.
   - Fresh pool, mismatched passphrases typed -> aborts with
     "passphrases do not match"; `cryptsetup isLuks /dev/sdX` reports
     non-LUKS (format did not run).
   - Fresh pool with `--passphrase-stdin < passphrase.txt` -> single
     read; no confirm.
   - Fresh pool with `--passphrase-file /path/to/file` -> single
     read; no confirm.
   - Fresh disk added to a live mounted pool -> single prompt (verify
     path handles typo protection).

## Out of scope

- In-loop retry on mismatch (one typo -> one re-run of `braid add`).
- Confirming in `braid replace` (already verifies against existing
  keyslot).
- Cross-referencing `pool_membership.disks` as an additional verify
  source when `pool.devices` is empty. The current design handles the
  "fresh disk into unassembled pool" case safely with an extra prompt;
  an enhancement to verify against a recorded-but-not-live member is a
  separate issue.
