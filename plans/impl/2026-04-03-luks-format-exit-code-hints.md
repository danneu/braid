# Plan: exit-code-aware errors for cryptsetup luksFormat

## Context

`luks_format` (cli/src/luks.rs:94-99) reports failures as a generic string: `"cryptsetup luksFormat failed (exit N): stderr"`. This was already fixed for `cryptsetup open` via `LuksError::OpenFailed` + `cryptsetup_open_hint()`. The same `translate_errno` exit code map (reference/cryptsetup/src/utils_tools.c:219-235) applies to luksFormat, but the hints need format-specific wording (e.g. exit 2 is "permission denied", never "wrong passphrase").

The current `cryptsetup_open_hint` hardcodes the exit-code-to-meaning map inline. Adding a second hint function would duplicate that map. Instead, introduce a shared classifier enum as single source of truth, then derive operation-specific hint text on top.

## Changes

All changes in `cli/src/luks.rs`.

### 1. Add shared `CryptsetupExitKind` enum + classifier

Insert above the existing `cryptsetup_open_hint` (~line 167):

```rust
/// Semantic classification of cryptsetup exit codes.
/// Single source of truth — maps cryptsetup's `translate_errno` (utils_tools.c).
enum CryptsetupExitKind {
    GenericFailure,    // exit 1: EINVAL / ENOENT / ENOSYS / default
    PermissionDenied,  // exit 2: EPERM
    OutOfMemory,       // exit 3: ENOMEM
    DeviceNotFound,    // exit 4: ENOTBLK / ENODEV
    DeviceBusy,        // exit 5: EEXIST / EBUSY
    Unknown,
}

fn classify_cryptsetup_exit(code: i32) -> CryptsetupExitKind {
    match code {
        1 => CryptsetupExitKind::GenericFailure,
        2 => CryptsetupExitKind::PermissionDenied,
        3 => CryptsetupExitKind::OutOfMemory,
        4 => CryptsetupExitKind::DeviceNotFound,
        5 => CryptsetupExitKind::DeviceBusy,
        _ => CryptsetupExitKind::Unknown,
    }
}
```

### 2. Refactor `cryptsetup_open_hint` to use classifier

Replace the current body (lines 172-181) to match on `CryptsetupExitKind` instead of raw ints. Same output strings, just routed through the shared enum. Compiler-enforced exhaustiveness means adding a new kind forces both hint functions to handle it.

```rust
fn cryptsetup_open_hint(exit_code: i32) -> &'static str {
    match classify_cryptsetup_exit(exit_code) {
        CryptsetupExitKind::GenericFailure => "generic failure",
        CryptsetupExitKind::PermissionDenied => "wrong passphrase or permission denied",
        CryptsetupExitKind::OutOfMemory => "out of memory",
        CryptsetupExitKind::DeviceNotFound => "device not found or not a block device",
        CryptsetupExitKind::DeviceBusy => "device is already open or busy",
        CryptsetupExitKind::Unknown => "unknown error",
    }
}
```

### 3. Add `cryptsetup_format_hint`

Same structure, format-specific wording where it differs (exits 2 and 5):

```rust
fn cryptsetup_format_hint(exit_code: i32) -> &'static str {
    match classify_cryptsetup_exit(exit_code) {
        CryptsetupExitKind::GenericFailure => "generic failure",
        CryptsetupExitKind::PermissionDenied => "permission denied (not root?)",
        CryptsetupExitKind::OutOfMemory => "out of memory",
        CryptsetupExitKind::DeviceNotFound => "device not found or not a block device",
        CryptsetupExitKind::DeviceBusy => "device busy or already formatted",
        CryptsetupExitKind::Unknown => "unknown error",
    }
}
```

### 4. Add `LuksError::FormatFailed` variant

Same shape as `OpenFailed` (line 28):

```rust
#[error("cryptsetup luksFormat failed for {device} (exit {exit_code}): {hint} — {stderr}")]
FormatFailed {
    device: String,
    exit_code: i32,
    hint: &'static str,
    stderr: String,
},
```

No downstream breakage: callers in add.rs:486 and replace.rs:221 use `?` through `#[from] LuksError`. The two matches in mount.rs:245/281 use `_` catch-all.

### 5. Update `luks_format` error path (line 94-99)

Replace `LuksError::Validation(format!(...))` with:

```rust
return Err(LuksError::FormatFailed {
    device: device.to_owned(),
    exit_code: result.exit_status,
    hint: cryptsetup_format_hint(result.exit_status),
    stderr: result.stderr.trim().to_owned(),
});
```

### 6. Tests

**Hint unit tests** (follow existing pattern from lines 400-426):

- `hint_format_exit_2_is_permission` — assert `cryptsetup_format_hint(2)` returns "permission denied (not root?)"
- `hint_format_exit_5_is_busy` — assert `cryptsetup_format_hint(5)` returns "device busy or already formatted"
- `hint_format_unknown_code` — assert `cryptsetup_format_hint(42)` returns "unknown error"

**Integration tests** (follow pattern from lines 433-510, using `MockRunner::with_output_stdin` from cmd.rs:877):

- `luks_format_exit_2_mentions_permission` — MockRunner returns exit 2 for `CryptsetupLuksFormat`, assert error contains "exit 2", "permission denied", and stderr
- `luks_format_exit_4_mentions_device_not_found` — MockRunner returns exit 4, assert error contains "exit 4" and "device not found"

## Files modified

- `cli/src/luks.rs` — shared classifier, refactored open hint, new format hint, new variant, updated error path, new tests

## Verification

```
just test-rust
```
