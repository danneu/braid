# Plan: Exit-code-specific error messages for cryptsetup open

## Context

`ensure_luks_open()` and `ensure_luks_open_with_key_file()` in `cli/src/luks.rs` report every non-zero cryptsetup exit as "Wrong passphrase?" / "Wrong keyfile?". But cryptsetup exit codes mean different things — exit 4 is "device not found", exit 5 is "device already open/busy", exit 2 is actually "wrong passphrase". Users hitting exit 4 or 5 get a misleading error that sends them down the wrong debugging path.

Additionally, the unlock path in `mount.rs` (lines 244, 273) uses `.map_err(|_| ...)` to unconditionally replace the `LuksError` with a "single-passphrase invariant violated" message — even for non-auth failures like device-not-found. This masks the real problem.

## Upstream exit code map

From `reference/cryptsetup/src/utils_tools.c:219-235` (`translate_errno`):

| Exit | Meaning |
|------|---------|
| 0 | success |
| 1 | generic failure (EINVAL, ENOENT, ENOSYS, unmapped) |
| 2 | wrong passphrase or permission denied (EPERM) |
| 3 | out of memory (ENOMEM) |
| 4 | device not found / not a block device (ENODEV, ENOTBLK) |
| 5 | device already open or busy (EBUSY, EEXIST) |

## Changes

### 1. Add structured error variant to `LuksError` (`cli/src/luks.rs:24-32`)

Add a new variant that carries exit code + stderr so callers can branch on it:

```rust
#[derive(Debug, thiserror::Error)]
pub enum LuksError {
    #[error("{0}")]
    Validation(String),
    #[error("cryptsetup open failed for {device} (exit {exit_code}): {hint} — {stderr}")]
    OpenFailed {
        device: String,
        exit_code: i32,
        hint: &'static str,
        stderr: String,
    },
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
```

### 2. Add hint helper (`cli/src/luks.rs`, before `ensure_luks_open`)

```rust
/// Map a cryptsetup exit code to a human-readable hint.
///
/// Exit codes from cryptsetup's `translate_errno` (utils_tools.c):
///   1 = generic failure, 2 = wrong passphrase / permission denied,
///   3 = out of memory, 4 = device not found, 5 = device already open / busy.
fn cryptsetup_open_hint(exit_code: i32) -> &'static str {
    match exit_code {
        1 => "generic failure",
        2 => "wrong passphrase or permission denied",
        3 => "out of memory",
        4 => "device not found or not a block device",
        5 => "device is already open or busy",
        _ => "unknown error",
    }
}
```

### 3. Update `ensure_luks_open` error (lines 181-186)

Replace `Validation(format!("...Wrong passphrase?..."))` with:

```rust
return Err(LuksError::OpenFailed {
    device: by_id.0.clone(),
    exit_code: result.exit_status,
    hint: cryptsetup_open_hint(result.exit_status),
    stderr: result.stderr.trim().to_owned(),
});
```

### 4. Update `ensure_luks_open_with_key_file` error (lines 231-236)

Same pattern as step 3.

### 5. Update `mount.rs` to branch on exit code (`cli/src/mount.rs:243-253`, `:272-281`)

Replace the unconditional `.map_err(|_| ...)` with a match on the error variant. Only produce the "invariant violated" message for auth failures (exit 2); propagate the original error otherwise.

**Passphrase path** (line 273):
```rust
luks::ensure_luks_open(runner, fs, name, by_id, &passphrase).map_err(|e| {
    match &e {
        LuksError::OpenFailed { exit_code: 2, hint, stderr, .. } => MountError::Failed(format!(
            "failed to open disk '{}': passphrase was verified \
             against '{}' but rejected here — {} ({}). \
             If the passphrase is correct, the single-passphrase \
             invariant may be violated by external LUKS manipulation",
            name, first_name, hint, stderr
        )),
        _ => MountError::Luks(e),
    }
})?;
```

**Keyfile path** (line 244): same pattern, mentioning keyfile instead of passphrase.

This preserves the invariant warning for auth failures while surfacing the upstream detail (`hint` + `stderr`), so a permission-denied failure won't be mistaken for a passphrase mismatch.

### 6. Fix mock exit codes in tests

Two tests model "wrong passphrase" as exit 5 (EBUSY), but upstream uses exit 2 (EPERM) for auth failures. Update:

- `cli/src/mount.rs:796` — change `5` → `2`
- `cli/src/unlock.rs:551` — change `5` → `2`

### 7. Add tests

**`cli/src/luks.rs` tests block** — unit tests for the hint helper (3 tests: exit 2, exit 5, unknown). Each with required intent/why/scenario block comments.

**`cli/src/luks.rs` tests block** — message-level tests via `MockRunner`:

`ensure_luks_open`:
- Exit 2: assert message contains "wrong passphrase", device name, and stderr
- Exit 4: assert message contains "device not found", device name, and stderr

`ensure_luks_open_with_key_file`:
- Exit 2: assert message contains "wrong passphrase", device name, and stderr
- Exit 4: assert message contains "device not found", device name, and stderr

**`cli/src/mount.rs` tests** — verify the existing `mount_passphrase_mismatch_names_disk` (line 747) test still passes with exit code 2 and still produces the invariant violation message.

**`cli/src/mount.rs` tests** — two new tests for non-auth propagation:
- Passphrase path: `CryptsetupLuksOpen` returns exit 4, assert `MountError::Luks(LuksError::OpenFailed { .. })` is preserved (not rewritten as invariant violation).
- Keyfile path: `CryptsetupLuksOpenKeyFile` returns exit 4, same assertion.

## Files to modify

| File | What changes |
|------|-------------|
| `cli/src/luks.rs` | Add `OpenFailed` variant, `cryptsetup_open_hint` helper, update both error sites, add tests |
| `cli/src/mount.rs` | Branch on `OpenFailed.exit_code` in both credential paths, fix mock exit code, add non-auth-failure test |
| `cli/src/unlock.rs` | Fix mock exit code from 5 → 2 |

## Verification

1. `cargo test -p braid-cli` — confirm no test regressions, new tests pass
2. `just test-rust` — full Rust test suite
