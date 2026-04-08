# Plan: warn on add/replace when pool has keyfile enrollment

## Context

When a pool has a keyfile enrolled in LUKS slot 1 (for USB auto-unlock) and
a user runs `braid add` or `braid replace` without `--enroll`, the new drive
only gets a passphrase in slot 0. Passphrase-based unlock still works, but
keyfile-based auto-unlock silently breaks on the new drive — `braid unlock
--key-file` fails when it hits the un-enrolled disk.

Today there's no detection or feedback. The user discovers the problem at
next boot when auto-unlock fails.

## Approach

Print a pre-confirm warning (not a hard block) when:
- Fresh LUKS formatting will happen (`PresentNotLuks` disk being added/replaced)
- `--enroll` was NOT passed
- Any live pool device has LUKS slot 1 occupied

The warning appears before the confirmation prompt so the user can abort or
proceed knowingly, then re-enroll afterward.

Detection is best-effort: probe/luksDump failures are swallowed with a brief
note so the warning path never aborts the command.

## Implementation

### 1. Add helper in `cli/src/luks.rs`

```rust
/// Best-effort check: does any live pool device have a keyfile in slot 1?
/// Scans all devices; returns true on first occupied slot 1.
/// Never fails the caller — probe errors are logged and skipped.
pub fn pool_has_keyfile_enrollment<R: CommandRunner>(
    runner: &R,
    devices: &[PoolDevice],
) -> bool
```

Uses `pool.devices` (already probed at add.rs:302 / replace.rs probe site)
instead of re-probing membership. Each `PoolDevice.underlying` is the raw
block device path — exactly what `check_key_slot` needs.

Logic:
- If `devices` is empty (bootstrap / unmounted) → return `false`
- For each device, call `check_key_slot(runner, &dev.underlying, LUKS_SLOT_KEYFILE)`
- If `Ok(Occupied)` → return `true`
- If `Err(_)` → `eprintln!("note: could not inspect slot 1 on {}: {e}", ...)` and continue
- After full scan → return `false`

Building blocks (all exist in `luks.rs`):
- `check_key_slot(runner, device, slot)` — reads LUKS header JSON, no unlock needed
- `LUKS_SLOT_KEYFILE` (= 1), `KeySlotState`

New import in `luks.rs`: `use crate::types::PoolDevice`

### 2. Wire into `braid add` (`cli/src/add.rs`)

Insert after the pool probe (line 311) and before the confirmation prompt
(line 357). Hoist the `any_needs_format` check from line 381:

```rust
let any_needs_format = probed
    .iter()
    .any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks));

if any_needs_format
    && params.enroll_key_file.is_none()
    && pool_has_keyfile_enrollment(runner, &pool.devices)
{
    eprintln!(
        "WARNING: Existing pool drives have a keyfile (keyslot-1) for auto-unlock, \
         but the new drive will not.\n  \
         Passphrase unlock still works, but the keyfile won't unlock the new drive \
         until it's enrolled.\n  \
         Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
    );
}
```

Reuse `any_needs_format` at line 381 instead of recomputing it.

Import: add `pool_has_keyfile_enrollment` to the existing `use crate::luks::{...}` block.

### 3. Wire into `braid replace` (`cli/src/replace.rs`)

Same pattern, but gate on `new_probed.state == PresentNotLuks` (not always
true — `replace` also handles `PresentLuks` replacement disks that don't need
fresh LUKS formatting). Insert before the confirmation prompt:

```rust
if matches!(new_probed.state, ConfigDiskState::PresentNotLuks)
    && params.enroll_key_file.is_none()
    && pool_has_keyfile_enrollment(runner, &pool.devices)
{
    eprintln!(
        "WARNING: Existing pool drives have a keyfile (keyslot-1) for auto-unlock, \
         but the new drive will not.\n  \
         Passphrase unlock still works, but the keyfile won't unlock the new drive \
         until it's enrolled.\n  \
         Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
    );
}
```

Import: add `pool_has_keyfile_enrollment` to the existing luks import block.

### 4. Unit tests in `cli/src/luks.rs`

Test the helper directly. Use `MockRunner` patterns from existing tests.

**Cases:**
1. No devices (empty vec) → returns `false`
2. One device, slot 1 occupied → returns `true`
3. One device, slot 1 empty → returns `false`
4. Two devices, first empty, second occupied → returns `true` (scans all)
5. `check_key_slot` errors on a device → logs note, continues, returns `false`

Mock responses: build `PoolDevice` values with `underlying` paths, wire
`MockRunner` with `CryptsetupLuksDump` responses (slot-1-empty and
slot-1-occupied JSON).

## Files to modify

| File | Change |
|------|--------|
| `cli/src/luks.rs` | Add `pool_has_keyfile_enrollment()` helper + tests |
| `cli/src/add.rs` | Call helper, print warning before confirm; hoist `any_needs_format` |
| `cli/src/replace.rs` | Call helper, print warning before confirm (gated on `PresentNotLuks`) |

## Verification

1. `just test-rust` — unit tests pass including new cases
2. Manual `braid add`: pool with enrollment + no `--enroll` → shows warning
3. Manual `braid add --enroll <dir>`: pool with enrollment → no warning
4. Manual `braid add`: pool without enrollment → no warning
5. Manual `braid replace`: pool with enrollment + `PresentNotLuks` + no `--enroll` → shows warning
6. Manual `braid replace`: pool with enrollment + `PresentLuks` replacement → no warning
7. Manual `braid replace --enroll <dir>` → no warning
