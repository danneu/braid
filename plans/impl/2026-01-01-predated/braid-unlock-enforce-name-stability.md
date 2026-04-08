# Plan: tighten degraded unlock with disk-map by_id identity validation

## Context

`cmd_unlock` classifies absent/bricked disks as degradable pool members by checking `disk_map.disks.contains_key(name)` — but never verifies that the disk-map entry's `by_id` matches the config's `by_id`. If someone changes a disk's `by_id` in NixOS config while the disk-map still has the old value, unlock would still allow degraded mount for a disk whose identity has drifted. This is a safety gap.

The fix is trivial: `validate_config_name_stability()` in `disk_map.rs:114-145` already validates by_id consistency between config and disk-map. It catches both reassignment (same name, different by_id) and rename (different name, same by_id). It's called by `add`, `remove`, `remove_missing`, and `replace` — but **not** by `unlock`.

## Changes

### 1. Add `NameStabilityError` to `UnlockError` (`cli/src/unlock.rs`)

Add a variant and `From` impl:

```rust
#[error("{0}")]
NameStability(#[from] crate::disk_map::NameStabilityError),
```

### 2. Remove "in v1.0" from `NameStabilityError` messages (`cli/src/disk_map.rs`)

Both `Reassignment` and `RenameDetected` error messages say "is not allowed in v1.0". Remove "in v1.0" — the constraint is not version-scoped, it's a permanent safety rule.

### 3. Call `validate_config_name_stability()` in `cmd_unlock` (`cli/src/unlock.rs`)

After the mountpoint check (line 51) and before the probe loop (line 53), add:

```rust
crate::disk_map::validate_config_name_stability(config, disk_map)?;
```

This fails hard (exit 1) on any config/disk-map identity mismatch — before any disk is probed or unlocked. `--allow-degraded` cannot bypass it since the check runs unconditionally before degraded classification.

### 4. Add unit tests (`cli/src/unlock.rs`)

**Test: `unlock_identity_mismatch_fails_even_with_allow_degraded`**
- Config has disk3 with by_id `virtio-disk3`
- Disk map has disk3 with by_id `virtio-disk3-OLD` (mismatch)
- disk3 is absent
- `allow_degraded: true`
- Expect: `UnlockError::NameStability(Reassignment { .. })`, NOT degraded path
- MockRunner only needs the mountpoint check mock (fails before probe)

**Test: `unlock_identity_mismatch_fails_without_allow_degraded`**
- Same setup, `allow_degraded: false`
- Same result — identity mismatch is always fatal

**Test: `unlock_identity_mismatch_fails_even_when_all_disks_healthy`**
- All 3 disks present, LUKS-formatted, mapper closed (normal healthy state)
- Disk map has disk1 with by_id `virtio-disk1-OLD` (mismatch vs config's `virtio-disk1`)
- `allow_degraded: false`
- Expect: `UnlockError::NameStability(Reassignment { .. })` — unlock refuses before probing
- MockRunner only needs the mountpoint check mock
- This proves identity enforcement is unconditional, not tied to degraded classification

### 5. Update decision doc (`docs/decisions/003-resilient-boot.md`)

Add to the Implementation section: `braid unlock` enforces disk-map/config identity for all unlocks — not only degraded scenarios. Any name reassignment or rename detected between config and disk-map is a hard error before any disk is probed or mounted. `--allow-degraded` only bypasses degraded-mount refusal, never identity mismatches.

### 6. NixOS VM test (`tests/cli/braid-unlock.py`)

Add two subtests:

**5a. Identity mismatch blocks degraded unlock:**
1. Save the current disk-map
2. Overwrite with a disk-map where one disk name has a wrong `by_id`
3. Run `braid unlock --allow-degraded`
4. Assert it fails with the identity mismatch error
5. Restore the original disk-map

**5b. Identity mismatch blocks healthy unlock:**
1. Save the current disk-map
2. Overwrite with one disk's `by_id` changed (all disks present and healthy)
3. Run `braid unlock`
4. Assert it fails with the identity mismatch error — proves enforcement is unconditional
5. Restore the original disk-map

## Files to modify

- `cli/src/unlock.rs` — add error variant, add validation call, add 3 unit tests
- `cli/src/disk_map.rs` — remove "in v1.0" from error messages
- `docs/decisions/003-resilient-boot.md` — one paragraph addition
- `tests/cli/braid-unlock.py` — 2 new subtests (with disk-map save/restore)

## Why up-front check instead of per-disk inline check

The user's original proposal checks by_id inside the Absent/PresentNotLuks classification branches. Calling `validate_config_name_stability()` up front is better:

1. **Less code** — one line vs inline checks in two branches
2. **Reuses existing tested function** — no duplicate logic
3. **Catches more** — also detects rename (by_id moved to different name)
4. **Fails faster** — no wasted time probing disks before failing

The observable behavior is identical for the stated requirements, and the up-front placement makes the enforcement unconditional — it applies to healthy unlocks too, not just degraded classification. This aligns with Principles 3 (safe-by-construction) and 5 (stable identifiers): if config and disk-map disagree, the user must resolve it explicitly before unlock will proceed.

## Verification

1. `just test-rust` — unit tests pass (including new mismatch tests)
2. `just test braid-unlock` — VM test passes (including new identity mismatch subtest)
3. Existing degraded tests still pass because their seeded disk-maps have matching by_id values
