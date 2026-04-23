# Fix verify_passphrase / verify_key_file failure-mode conflation

## Context

`luks::verify_passphrase` (`cli/src/luks.rs:171-183`) and `luks::verify_key_file`
(`cli/src/luks.rs:386-396`) currently return `Ok(result.exit_status == 0)` --
collapsing every non-zero cryptsetup exit into "verification failed".
Downstream, `explain_open_failure` (`cli/src/mount.rs:351-375`) is invoked with
`LuksHeaderState::Ok` as its fallback path, producing user-facing text like
`"wrong passphrase (verified against {first_name})"`.

The result: when `cryptsetup open --test-passphrase` exits with EBUSY (5),
ENODEV (4), or a generic EINVAL (1) -- conditions that have nothing to do with
the credential -- the CLI tells the user their passphrase is wrong. This is
actively misleading during incident recovery (e.g. a stale mapper holding the
device busy looks like a lockout), and it contradicts the invariant that exit
2 (EPERM) is the only cryptsetup exit that semantically means "wrong
credential" (verified in `reference/cryptsetup/src/utils_tools.c:219-235`
translate_errno).

The fix: introduce a `VerifyOutcome { Authenticated, Rejected }` enum. Map
exit 0 -> `Authenticated`, exit 2 -> `Rejected`, everything else ->
`LuksError::OpenFailed` (the same variant that `ensure_luks_open` already
emits, so the hint/stderr narration is consistent). This keeps each arm's
fail-closed stance tied to what the failure actually means, per the project
guidance in `feedback_fail_closed_by_downstream_blast_radius.md`.

## Critical files

- `cli/src/luks.rs` -- new `VerifyOutcome` enum; rewrite bodies of
  `verify_passphrase` (line 171) and `verify_key_file` (line 386).
- `cli/src/mount.rs` -- 2 callsites (line 389, line 494). The `!ok` branch
  becomes the `Rejected` arm of a `match`. Non-auth exits must NOT bypass
  header diagnosis: catch `LuksError::OpenFailed` from the verify call,
  run `probe_luks_header`, and route through `explain_open_failure` with
  `MountError::Luks(e)` as the `Ok` fallback. This preserves the existing
  unreadable/damaged-header guidance for verification failures -- if the
  backing device was scribbled over, we still want to say so rather than
  "cryptsetup open failed for {device} (exit 1): generic failure".
- `cli/src/add.rs` -- 1 callsite (line 429). `Rejected` keeps the existing
  `AddError::Validation("passphrase does not match existing pool member...")`
  wording; non-auth exits now surface via `AddError::Luks` (already
  `#[from] LuksError`).
- `cli/src/replace.rs` -- 1 callsite (line 203). Same shape as add.
- `cli/src/enroll_key_file.rs` -- 2 callsites (line 77 passphrase,
  line 88 keyfile). Line 77 keeps
  `EnrollKeyFileError::Validation("wrong passphrase (verified against ...)")`
  for `Rejected`. Line 88 is the idempotency check: `Authenticated` means
  "already enrolled", `Rejected` means "not yet enrolled, proceed"; any other
  exit must NOT be silently treated as "not enrolled" (that was the silent
  bug here) -- let `LuksError::OpenFailed` propagate.

## Implementation sketch

### 1. `cli/src/luks.rs` -- add enum + classify exits

```rust
/// Outcome of a keyslot verification attempt. Exit 2 (EPERM) is the only
/// cryptsetup exit that semantically means "wrong credential"; every other
/// non-zero exit is a real error (busy/missing/OOM/generic) and must not be
/// silently treated as rejection by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    Authenticated,
    Rejected,
}

pub fn verify_passphrase<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &str,
) -> Result<VerifyOutcome, LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupTestPassphrase { device: device.to_owned() },
        passphrase.as_bytes(),
    )?;
    classify_verify_exit(device, &result)
}

pub fn verify_key_file<R: CommandRunner>(
    runner: &R,
    device: &str,
    key_file_path: &std::path::Path,
) -> Result<VerifyOutcome, LuksError> {
    let result = runner.run(&CmdRequest::CryptsetupTestKeyFile {
        device: device.to_owned(),
        key_file_path: key_file_path.display().to_string(),
    })?;
    classify_verify_exit(device, &result)
}

fn classify_verify_exit(device: &str, result: &RawCommandOutput) -> Result<VerifyOutcome, LuksError> {
    match result.exit_status {
        0 => Ok(VerifyOutcome::Authenticated),
        2 => Ok(VerifyOutcome::Rejected),
        code => Err(LuksError::OpenFailed {
            device: device.to_owned(),
            exit_code: code,
            hint: cryptsetup_open_hint(code),
            stderr: result.stderr.trim().to_owned(),
        }),
    }
}
```

Per `feedback_tests_bind_to_real_mapping.md`, `classify_verify_exit` is the
named helper so tests bind to the real mapping, not a hand-built variant.

### 2. Update 6 callsites

For `add.rs:429`, `replace.rs:203`, `enroll_key_file.rs:77`, the mechanical
rewrite applies: `let ok = verify_*(...)?; if !ok { X }` becomes

```rust
match luks::verify_passphrase(runner, ...)? {
    VerifyOutcome::Authenticated => {}
    VerifyOutcome::Rejected => { X }
}
```

Non-auth exits now surface via the error enums' `Luks(#[from] LuksError)`
variant (already declared on `AddError`, `ReplaceError`, `MountError`,
`EnrollKeyFileError`).

For `enroll_key_file.rs:88` (the idempotency check), rewrite to a `match`
with both arms preserved: `Authenticated` -> already-enrolled branch,
`Rejected` -> proceed to slot-1 preflight. Non-auth exits propagate via `?`
instead of being silently treated as "not enrolled".

For `mount.rs:389` and `mount.rs:494` (the unlock paths), the verify call
must preserve header diagnosis on non-auth failures. Reshape to:

```rust
let outcome = match luks::verify_passphrase(runner, &first_by_id.0, passphrase) {
    Ok(o) => o,
    Err(e @ LuksError::OpenFailed { .. }) => {
        let original_summary = format!("verify failed on '{first_name}': {e}");
        let header_state = luks::probe_luks_header(runner, &first_by_id.0);
        return Err(explain_open_failure(
            first_name,
            &first_by_id.0,
            header_state,
            &original_summary,
            MountError::Luks(e),
        ));
    }
    Err(e) => return Err(e.into()),
};
match outcome {
    VerifyOutcome::Authenticated => {}
    VerifyOutcome::Rejected => { /* existing Rejected-path narration */ }
}
```

Same shape at the keyfile callsite (line 494). This keeps the unreadable /
damaged header guidance reachable when verification itself hits a non-auth
failure (e.g. backing device wiped -> exit 1 -> probe finds
`LuksHeaderState::Unreadable` -> user sees off-system-backup guidance, not
"generic failure").

### 3. Tests

Per `feedback_tests_bind_to_real_mapping.md` and
`feedback_test_at_failure_layer.md`, every forced non-auth exit needs a test
that fails if classification is reverted:

- **`cli/src/luks.rs` (new `#[cfg(test)]` cases)** -- call
  `classify_verify_exit` directly with synthetic `CmdResult`s:
  - exit 0 -> `Ok(Authenticated)`
  - exit 2 -> `Ok(Rejected)`
  - exit 1, 3, 4, 5 -> each a distinct test returning
    `Err(LuksError::OpenFailed { exit_code: N, hint, .. })`, asserting the
    hint string from `cryptsetup_open_hint` is preserved.

- **`cli/src/mount.rs` (four new VM-free unit tests)** -- beside the
  existing `explain_open_failure_*` suite (mount.rs:1975-2094). The
  behavior split to lock down is: on non-auth verify exit, the outcome
  branches on header state -- `LuksHeaderState::Ok` must surface
  `MountError::Luks(OpenFailed)` verbatim, while
  `Unreadable`/`Damaged` must surface the guidance text from
  `explain_open_failure`. Both credential types must be covered:

  1. **Passphrase, exit 5 + healthy header** -- drive
     `open_disks_with_passphrase` with `CryptsetupTestPassphrase` exit 5
     (EBUSY), plus isLuks+luksDumpText both exit 0. Assert the error
     is `MountError::Luks(LuksError::OpenFailed { exit_code: 5, .. })`
     and the message does NOT contain "wrong passphrase".
  2. **Passphrase, exit 1 + unreadable header** -- same drive, but
     `CryptsetupTestPassphrase` exit 1 and `CryptsetupIsLuks` exit 1
     (magic gone). Assert the error contains the
     `luks_header_unreadable_guidance()` string and does NOT contain
     "wrong passphrase" or "generic failure".
  3. **Keyfile, exit 5 + healthy header** -- drive `execute_open_plan`
     keyfile arm with `CryptsetupTestKeyFile` exit 5, isLuks+luksDumpText
     exit 0. Assert `MountError::Luks(OpenFailed { exit_code: 5, .. })`
     and no "wrong keyfile" text.
  4. **Keyfile, exit 1 + unreadable header** -- `CryptsetupTestKeyFile`
     exit 1, `CryptsetupIsLuks` exit 1. Assert the unreadable-header
     guidance is emitted.

  Together these pin the high-severity concern (header diagnosis still
  reachable) and the central misdiagnosis bug, across both credential
  types.

- **`cli/src/enroll_key_file.rs` (new unit test)** -- cover the silent
  bug at line 88. Mock `CryptsetupTestPassphrase` exit 0 (passphrase
  check passes), then `CryptsetupTestKeyFile` exit 5 (EBUSY). Assert:
  (a) the returned error is `EnrollKeyFileError::Luks(
  LuksError::OpenFailed { exit_code: 5, .. })`, and (b) no
  `CryptsetupLuksDump` / slot-1 preflight command mock was consumed --
  i.e. the flow stopped instead of silently treating the disk as
  "not enrolled". Without this, an enroll-layer revert reintroduces
  the silent misclassification even if the mount-layer tests still pass.

- **`cli/src/add.rs` and `cli/src/replace.rs` -- compile-only callsite
  coverage** -- neither `add::tests` (add.rs:871+) nor `replace::tests`
  (replace.rs:762+) currently mock `CryptsetupTestPassphrase`; their
  existing harnesses are built around pool/device flows, not the
  verify-then-open preflight. Spinning up a new mock harness for a
  2-line callsite change is disproportionate. These two callsites rely
  on: (a) the `classify_verify_exit` unit tests in luks.rs proving the
  mapping exit 5 -> `OpenFailed` works, and (b) the compile-time
  signature change (`bool` -> `VerifyOutcome`) forcing the `match`
  rewrite. Explicitly documented here so a future reviewer knows this
  gap is deliberate, not overlooked.

- **Existing test audit**: The Explore pass confirmed that every existing
  test (mount.rs:2214-2430, enroll_key_file.rs:772-826) uses exit 2 for
  rejection mocks, so no flips are required. The four
  `explain_open_failure_*` unit tests at mount.rs:1976-2094 call the helper
  directly with a seeded `LuksHeaderState` and do not touch verify; they
  still pass unchanged.

### 4. Scope non-goals

- Do NOT touch `probe_luks_header` or `LuksHeaderState` -- the
  `ProbeFailed`/`Unreadable`/`Damaged`/`Ok` classification is correct and
  orthogonal to this fix.
- Do NOT add a "maybe it's corrupt, maybe it's busy" narrative for exit 1.
  Per `feedback_no_diagnostic_refinements_in_mutation_paths.md`, we propagate
  the structured `OpenFailed` with its hint + stderr and let the user (or
  `braid doctor`) investigate. No widening of outcome enums beyond
  Authenticated/Rejected.

## Verification

1. `just test-rust` -- new `classify_verify_exit` tests pass; the
   passphrase/keyfile non-auth regression test in mount.rs fails without the
   fix, passes with it.
2. `cargo build -p braid` -- confirms all 6 callsites compile against the
   new signature.
3. `just test-vm` -- the existing VM suite (including
   `unlock_passphrase_verify_fails_*` tests at mount.rs:2214/2335/2388)
   continues to pass; these tests use exit 2 for rejection paths.
4. Manual smoke: in a NixOS VM, `cryptsetup close` a backing device then
   run `braid unlock` -- confirm the error mentions "device busy" /
   `OpenFailed` wording, not "wrong passphrase".
