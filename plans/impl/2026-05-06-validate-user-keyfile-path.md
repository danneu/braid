# Plan: enforce the 4096-byte regular-file contract for user-supplied keyfiles

## Context

braid's binary keyfile is contractually 4096 bytes: `generate_key_file`
reads exactly that from `/dev/urandom` into a
`Zeroizing<[u8; KEYFILE_SIZE]>` buffer
(`cli/src/enroll_key_file.rs:323-329`); `KEYFILE_SIZE` lives at
`cli/src/luks.rs:22` and `LuksError` at `cli/src/luks.rs:51`. The
cryptsetup invocations pin `--keyfile-size 4096` /
`--new-keyfile-size 4096` (`cli/src/cmd.rs:737-770`, with rationale and
asymmetry guards at `cli/src/cmd.rs:1897-2111`), and the docs reflect
the same number (`docs/luks-unlock.md`, `manual/commands/enroll.md:20`).

A finding initially flagged the validator at
`cli/src/enroll_key_file.rs:506-538` as a "silent enrollment of an
8-byte derived key" security regression. Verification against
`reference/cryptsetup/lib/utils.c:314-317` shows cryptsetup itself
strictly enforces the exact byte count when `--keyfile-size N != 0`
(EINVAL -> exit 1 -> stderr `Cannot read requested amount of data.`),
so disk-level keys are never derived from a short file. The real
problems are UX and consistency:

- The braid layer accepts any-size file at the validation boundary, so
  users see a cryptic `exit 1: generic failure -- Cannot read requested
  amount of data.` from cryptsetup instead of a clear "keyfile must be
  exactly 4096 bytes" message.
- `braid unlock --key-file PATH` bypasses validation entirely
  (`credential::resolve_credential` at `cli/src/credential.rs:48-58`
  only stores the path), so the cryptic error surfaces from that
  command too.
- `braid recover` has no `--key-file` flag, but it replays journaled
  `add --enroll` / `replace --enroll` operations whose journal carries
  the original keyfile path. `ensure_keyfile_enrolled` at
  `cli/src/recover.rs:1966-1979` then calls `luks::verify_key_file` and
  `luks::enroll_key_file` directly against that path. If the journaled
  file has been replaced, truncated, or symlinked since the original
  command, recovery surfaces the same cryptic cryptsetup error with no
  early validation.
- `is_file()` follows symlinks, so `braid.key -> /etc/passwd` is
  accepted at validation. The NixOS auto-unlock module guards CWE-59
  at `modules/braid/storage.nix:227-248`; the CLI does not, leaving
  the two attack surfaces inconsistent.

Goal: a single shared validator -- exists, regular file (no symlink),
size == `KEYFILE_SIZE` -- called from every command that takes a
user-supplied keyfile path, so all four affected paths (enroll,
add --enroll, replace --enroll, unlock --key-file) and the recovery
replay surface fail with the same 4096-byte validation detail before
any cryptsetup keyfile verify / open / enroll invocation runs against
the file. (The outer error envelope still differs by command --
`enroll`/`recover` add a `luks error:` prefix, `unlock` shows the
raw `LuksError`, `add`/`replace` propagate via `e.to_string()` --
which is fine: tests assert the inner `4096` detail and the
"validation runs before cryptsetup" contract, not byte-identical
output.)

## Approach

Extract `luks::validate_user_keyfile_path` and plumb it through the
three integration points that consume a user-supplied keyfile path:

1. `enroll_key_file::validate_key_file_path` -- the planning-time
   check shared by `enroll`, `add --enroll`, and `replace --enroll`.
2. `credential::resolve_credential` -- the credential-resolution
   boundary used by `cmd_unlock`.
3. `recover::ensure_keyfile_enrolled` -- the recovery replay
   helper that consumes a journaled `add --enroll` /
   `replace --enroll` keyfile path.

Reuse the existing `LuksError::Validation` variant and the existing
`#[from] LuksError` derives on `EnrollKeyFileError`, `MountError`,
and `RecoverError`, so error mapping falls out of `?` at each
integration point.

## Critical files

- `cli/src/luks.rs:22` (`KEYFILE_SIZE`) and `LuksError` enum at line
  51 -- new helper lives here.
- `cli/src/enroll_key_file.rs:506-538` (`validate_key_file_path`) --
  non-generate branch delegates to the helper.
- `cli/src/credential.rs:48-58` (`resolve_credential`) -- gain a
  one-line validation before constructing `OpenCredential::KeyFile`.
  Covers `cmd_unlock` (`cli/src/unlock.rs:116-121`, which maps the
  result via `.map_err(MountError::from)`); recover passes `None` for
  the keyfile slot today (`cli/src/recover.rs:831-836`), so this
  hook covers unlock alone.
- `cli/src/recover.rs:1966-1979` (`ensure_keyfile_enrolled`) -- gain
  a one-line validation at the top, before
  `luks::verify_key_file` / `luks::enroll_key_file` run.
- `cli/src/enroll_key_file.rs:1237-1242` (`make_existing_keyfile` test
  helper) and a few tests that use small fixtures.
- `tests/cli/braid-unlock-key-file.py` -- gains a wrong-size subtest.

## Step 1: helper in `cli/src/luks.rs`

Add next to `KEYFILE_SIZE`:

```rust
/// Single source of truth for "is this user-supplied path a valid
/// braid keyfile" -- exists, regular file (no symlink resolution),
/// exactly KEYFILE_SIZE bytes. Shared so enroll/add/replace and
/// unlock/recover surface one validation detail, and so a wrong-
/// size or symlinked file fails at the validation boundary instead
/// of falling through to cryptsetup's `Cannot read requested amount
/// of data` error.
pub fn validate_user_keyfile_path(path: &Path) -> Result<(), LuksError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LuksError::Validation(format!("keyfile not found: {}", path.display()))
        } else {
            LuksError::Validation(format!("cannot read keyfile {}: {e}", path.display()))
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(LuksError::Validation(format!(
            "keyfile must be a regular file, not a symlink: {}",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(LuksError::Validation(format!(
            "keyfile is not a regular file: {}",
            path.display()
        )));
    }
    if meta.len() != KEYFILE_SIZE as u64 {
        return Err(LuksError::Validation(format!(
            "keyfile must be exactly {} bytes, got {} bytes: {}",
            KEYFILE_SIZE,
            meta.len(),
            path.display()
        )));
    }
    Ok(())
}
```

`symlink_metadata` is the load-bearing call -- it does NOT follow the
link, so a symlink is detected before `is_file()` masks it. The doc
comment is required by `AGENTS.md` (top-level `pub` Rust item).

## Step 2: enroll path

In `cli/src/enroll_key_file.rs:517-536`, replace the manual
`exists`/`metadata`/`is_file` block in the `else` branch with a single
call:

```rust
} else {
    luks::validate_user_keyfile_path(key_file_path)?;
}
```

`#[from] LuksError` on `EnrollKeyFileError` (`cli/src/enroll_key_file.rs:28`)
maps the variant cleanly. The generate branch keeps its existing
`exists()` check -- it asserts the file does NOT exist, semantics the
helper does not cover.

## Step 3: unlock path

In `cli/src/credential.rs:48-58`, validate inside `resolve_credential`:

```rust
pub fn resolve_credential(
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    key_file: Option<&Path>,
) -> Result<OpenCredential, LuksError> {
    if let Some(kf) = key_file {
        luks::validate_user_keyfile_path(kf)?;
        return Ok(OpenCredential::KeyFile(kf.to_path_buf()));
    }
    let pp = luks::read_passphrase(passphrase_file, passphrase_stdin)?;
    Ok(OpenCredential::Passphrase(pp))
}
```

The function already returns `Result<OpenCredential, LuksError>`
(post the credential-module split), so the helper's `LuksError`
propagates directly via `?` with no wrapping. `cmd_unlock` maps that
to `MountError` via the existing `.map_err(MountError::from)` at
`cli/src/unlock.rs:121` (`#[from] LuksError` on `MountError` at
`cli/src/mount.rs:21` carries the conversion); no edits needed at
the call site.

`luks::read_passphrase` already returns `crate::secret::Passphrase`
(`luks.rs:7` imports that type from `cli/src/secret.rs`), so the
passphrase arm is unchanged -- the only edit is the new validator
call inside the `Some(kf)` branch.

Boundary note: this hook fires inside `UnlockPlan::execute`, after
`plan_unlock` has already run `mount::plan_open_pool` (which probes
membership). The validator is therefore "before any cryptsetup
keyfile open / verify / enroll invocation against the file", not
"before any disk probe". The two gates that legitimately bypass
`resolve_credential` -- `plan.to_unlock.is_empty()` (every mapper
already open; `cli/src/unlock.rs:113`) and `DegradedRefused` exits
before execute -- never invoke cryptsetup against the keyfile, so
skipping validation in those branches is correct: there is no
cryptic-error surface to defend.

## Step 4: recovery replay path

In `cli/src/recover.rs:1966-1979`, validate at the top of
`ensure_keyfile_enrolled` before either cryptsetup call:

```rust
fn ensure_keyfile_enrolled<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &Passphrase,
    key_file: &std::path::Path,
) -> Result<(), RecoverError> {
    luks::validate_user_keyfile_path(key_file)?;
    match luks::verify_key_file(runner, device, key_file)? {
        VerifyOutcome::Authenticated => Ok(()),
        VerifyOutcome::Rejected => {
            luks::enroll_key_file(runner, device, passphrase, key_file)?;
            Ok(())
        }
    }
}
```

The `passphrase: &Passphrase` parameter is the post-secret-handling
shape (`crate::secret::Passphrase`, see `cli/src/recover.rs:1969`).
Both call sites at `cli/src/recover.rs:2186` and `2479` already pass
`passphrase.expose_secret()` from a `RecoverPassphrase` (defined at
`cli/src/recover.rs:49-58`), and `RecoverPassphrase::expose_secret`
returns `&Passphrase`, so no caller changes are needed.

`RecoverError` already wraps `LuksError` via `#[from]`
(`cli/src/recover.rs:40`). The helper is called per-device inside the
journaled add / replace replay loops (lines 2186 and 2479);
validating once per call is a single `stat()` and the "first failing
device aborts replay" shape is unchanged. Centralizing here -- rather
than at the loop sites -- keeps the recovery boundary co-located with
the only place the file is consumed.

## Step 5: existing tests

Tests on the validation path move to 4096 bytes. Tests outside the
validation path stay.

| File | Line | Today | Action |
|------|------|-------|--------|
| `cli/src/enroll_key_file.rs` | 1246-1248 | `make_existing_keyfile` writes `b"keyfile-data"` (12 B) | Write `vec![0u8; luks::KEYFILE_SIZE]`. Tests using this helper exercise planning/probe paths, not real cryptsetup, so zeros are fine. |
| `cli/src/enroll_key_file.rs` | 1504 | `validate_existing_keyfile_accepts_regular_file_without_mountpoint` writes `b"existing"` (8 B) | Write 4096 zeros. Add a sibling test asserting an 8-byte file is rejected with a message containing "4096". |
| `cli/src/enroll_key_file.rs` | 1435, 2584, 2621 | 8 / 8 / 5-byte fixtures in `--generate` rejection tests | Leave -- generate branch only checks `exists()`. |
| `cli/src/recover.rs` | 6486, 7311, 7718 | `ensure_keyfile_enrolled_is_idempotent_and_fails_on_probe_errors`, `replace_pool_mutation_fresh_luks_expected_label_finishes_prep_only`, `replace_pool_mutation_fresh_luks_header_backup_failure_preserves_journal` -- use `/run/keys/braid.key` / `/run/keys/braid-new.key` literals and rely on mocked `CryptsetupTestKeyFile` / `CryptsetupLuksAddKeyFile` behavior | Replace literal paths with a `tempfile::TempDir` + 4096-byte file written via `std::fs::write(path, vec![0u8; luks::KEYFILE_SIZE])`. The test still feeds the same path string to the `MockRunner` so the existing mock matches by argv survive unchanged. The first test has three sub-scenarios (accepted / rejected / busy) that all need the same fixture. |
| `cli/src/recover.rs` | 7477, 7553, 7605, 4038, 11911 | `replace_pool_mutation_fresh_luks_wrong_label_preserves_journal`, `_absent_target_preserves_journal`, `_bad_passphrase_preserves_journal`, `mixed_pool_mutation_add_journal` constructor, `plan_recover_dry_run_pool_mutation_already_mounted_all_live_mixed_modes_renders_safe_placeholders` | Leave -- all of these either fail before `ensure_keyfile_enrolled` is reached (wrong label / absent target / bad passphrase / all-live skips fresh-target prep) or only render the journal as text. The path literal never reaches the validator, so changing it would be churn without behavioral coverage. |
| `tests/cli/braid-enroll.py` | 443 | `bs=32 count=1 -> /tmp/conflict.key` | Leave -- fed directly to `cryptsetup luksAddKey` (no `--new-keyfile-size`) to simulate external slot occupation; never enters braid as a keyfile. |
| `tests/cli/braid-enroll-generate.py` | 126 | same | Leave -- same reasoning. |

## Step 6: new tests

1. **Rust unit tests in `cli/src/luks.rs`** for
   `validate_user_keyfile_path`. Each follows the project's
   Intent / Why it exists / Scenario preamble (`AGENTS.md` "Test
   Conventions"; preamble form in `docs/testing.md`):
   - exact 4096 bytes -> Ok.
   - 0 / 4095 / 4097 bytes -> `LuksError::Validation` whose message
     contains both "4096" and the actual byte count.
   - symlink to a 4096-byte file -> rejected with "must be a regular
     file, not a symlink".
   - missing path -> "keyfile not found".
   - directory -> rejected.

2. **Rust unit test in `cli/src/credential.rs`** asserting
   `resolve_credential` with a wrong-size keyfile returns
   `Err(LuksError::Validation(_))` whose message names "4096", and
   that no `CommandRunner` work is needed to produce it. (The
   `cmd_unlock` mapping to `MountError::Luks` via
   `.map_err(MountError::from)` is exercised by the VM subtest in
   step 6.4 below; no separate mount-side unit test is needed.)

3. **Rust unit test in `cli/src/recover.rs`** asserting
   `ensure_keyfile_enrolled` with a wrong-size journaled keyfile
   returns `RecoverError::Luks(LuksError::Validation(_))` whose
   message names "4096", and that the test's `CommandRunner` mock
   records *no* `CryptsetupTestKeyFile` or `CryptsetupLuksAddKeyFile`
   invocation. This pins the "validate before cryptsetup" contract on
   the recovery replay path. Pattern: build a `tempfile::TempDir`,
   write an 8-byte file at `dir.path().join("braid.key")`, build a
   `Passphrase` via the existing `passphrase("testpass")` test helper
   at `cli/src/recover.rs:3080`, pass both into
   `ensure_keyfile_enrolled` along with a `MockRunner::default()`
   (no `with_output` registrations), assert the error variant and
   that `runner.requests()` is empty. This sits next to the existing
   `ensure_keyfile_enrolled_is_idempotent_and_fails_on_probe_errors`
   at `cli/src/recover.rs:6486` (which Step 5 updates to use a real
   4096-byte temp fixture) so both the happy path and the
   wrong-size rejection path live together.

4. **NixOS VM subtest in `tests/cli/braid-unlock-key-file.py`** added
   *after* the existing Test 2 "wrong keyfile rejected" subtest so the
   pre-existing 4096-byte `/tmp/wrong.key` fixture (created at line 55)
   is left intact:

   ```python
   # Intent: braid unlock --key-file rejects a wrong-size keyfile at
   #   the CLI validation boundary, before any cryptsetup keyfile
   #   verify / open invocation runs.
   # Why it exists: prior to the shared validator, unlock surfaced
   #   cryptsetup's `Cannot read requested amount of data.` (exit 1,
   #   "generic failure") instead of a clear braid-level error. This
   #   pins the new boundary against a real binary, complementing the
   #   wrong-content (auth-rejected) coverage in Test 2 above.
   # Scenario: an admin points --key-file at an undersized placeholder
   #   (e.g. `printf short > /tmp/wrong-size.key`) and must see a
   #   message that names the 4096-byte contract.
   with subtest("Test 2c: wrong-size keyfile rejected with clear error"):
       close_all()
       machine.succeed("printf 'short' > /tmp/wrong-size.key")
       ret = machine.execute("braid unlock --key-file /tmp/wrong-size.key 2>&1")
       assert ret[0] == 1, f"expected exit 1 for wrong-size keyfile, got {ret[0]}"
       assert "4096" in ret[1], (
           f"error must name 4096-byte contract, got: {ret[1]!r}"
       )
       machine.fail("mountpoint -q /mnt/storage")
   ```

   Use `/tmp/wrong-size.key`, *not* `/tmp/wrong.key`, so the
   auth-rejection fixture stays usable by Test 2 and Test 2b above.

   Symlink rejection is already covered by the Rust test layer -- no
   need to spend a VM-test cycle on it unless we discover a regression.

## Verification

- `just test-rust` -- new helper unit tests, updated fixtures.
- `just test-vm braid-enroll braid-add-enroll braid-unlock-key-file`
  -- regression check + new subtest.
- `just test-vm replace-preview-warnings replace-live-disk` --
  confirm replace --enroll path still validates as expected.
- `just test-parsers` -- unaffected by the change but rerun to
  confirm the parser canary stays green.

## Out of scope

- The `--keyfile-size 4096` / `--new-keyfile-size 4096` argv pins in
  `cli/src/cmd.rs:737-770` are correct and stay.
- `KEYFILE_SIZE = 4096` is shared with the NixOS auto-unlock module
  and is not changing.
- The auto-unlock module's CWE-59 realpath defense at
  `modules/braid/storage.nix:227-248` already exceeds what the CLI is
  gaining and is not modified.
