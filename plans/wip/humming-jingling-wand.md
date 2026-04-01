# Fatal LUKS UUID cross-check during unlock

## Context

Pool.json may already contain authoritative `luks_uuid` values — populated by `add`, `replace`, `recover`, and `unlock`'s `refresh_pool_metadata`. Today, `unlock` (and `recover`, which shares `open_and_mount_pool`) never compares the stored UUID against the probed UUID from `cryptsetup luksUUID`. A disk could be silently swapped and braid would attempt to mount the wrong data.

Multi-disk pools get implicit protection from btrfs FSID-based assembly (kernel rejects mismatched FSIDs), but the failure surfaces as a cryptic btrfs mount error rather than a clear diagnostic. Single-disk pools have no protection at all.

The fix: when pool.json already has a `luks_uuid` for a member, compare it against the probed UUID. On mismatch, fatal error — no `--force`, no override. The error states the fact (which disk, expected vs found UUID) without prescribing a remedy, since the cause could be a drive swap, LUKS reformat, header corruption, or wrong drive plugged in.

## Files to modify

- `cli/src/mount.rs` — add UUID cross-check in the probe loop, add two failing tests
- `docs/principles.md` — update Principle 3 identity check list
- `docs/decisions/runtime-disk-membership.md` — document UUID enforcement in state contract
- `README.md` — document UUID mismatch behavior in Pool unlock section

## Implementation

### 1. Write failing tests (TDD)

Add two tests to `cli/src/mount.rs`:

**Test A: `mount_luks_uuid_mismatch_closed`** — mismatch on a closed LUKS device.

Setup:
- `two_disk_membership()` with disk1's `luks_uuid` set to `"aaaaaaaa-1111-2222-3333-444444444444"`
- MockRunner: MountpointCheck (not mounted) + CryptsetupLuksUuid for disk1 returns **different** UUID `"ffffffff-ffff-ffff-ffff-ffffffffffff"` + CryptsetupLuksUuid for disk2 returns its normal UUID
- MockFs: both by-id paths exist, no mappers open

Assertions:
- Result is `Err`
- Error message contains `"disk1"` (which disk)
- Error message contains `"aaaaaaaa"` (expected UUID)
- Error message contains `"ffffffff"` (found UUID)

**Test B: `mount_luks_uuid_mismatch_already_open`** — mismatch on an already-open mapper.

Setup:
- `two_disk_membership()` with disk1's `luks_uuid` set to `"aaaaaaaa-1111-2222-3333-444444444444"`
- MockRunner: MountpointCheck (not mounted) + CryptsetupLuksUuid for disk1 returns **different** UUID `"ffffffff-ffff-ffff-ffff-ffffffffffff"` + CryptsetupLuksUuid for disk2 returns its normal UUID
- MockFs: both by-id paths exist, **disk1's mapper is open** (`/dev/mapper/braid-disk1` exists)

Assertions: same as Test A — the check fires regardless of mapper state.

Both tests will fail until the check is implemented: today's code ignores the UUID and proceeds to later steps (passphrase verification or scan+mount), hitting unrelated errors.

### 2. Add UUID cross-check

In `open_and_mount_pool` (mount.rs), merge the two `PresentLuks` match arms into one and add the UUID comparison before the mapper_open branch:

```rust
ConfigDiskState::PresentLuks { uuid, mapper_open } => {
    if let Some(expected) = &member.luks_uuid {
        if expected != uuid {
            return Err(MountError::Failed(format!(
                "disk '{}' LUKS UUID mismatch at {}:\n  \
                 expected  {}\n  \
                 found     {}",
                name, member.by_id, expected, uuid
            )));
        }
    }

    if *mapper_open {
        eprintln!("{}  disk: {:<10}already open", tag("ok"), name);
        any_open = true;
    } else {
        eprintln!("{}  disk: {:<10}found", tag("ok"), name);
        to_unlock.push((name.clone(), member.by_id.clone()));
    }
}
```

This replaces the current two separate arms (lines 79-91).

### 3. Update docs

**`docs/principles.md`** — Principle 3 (line 19): add LUKS UUID cross-check to the identity check list. Currently reads:

> a multi-layer identity check (LUKS label match, pool-mounted requirement, btrfs FSID comparison) prevents accidental data loss

Update to include LUKS UUID verification during unlock.

**`docs/decisions/runtime-disk-membership.md`** — State contract section (line 54): currently reads:

> If pool.json is readable but stale (a member fails to probe), unlock warns and proceeds with the members it can probe.

Add a bullet after this line documenting: if a member's stored `luks_uuid` doesn't match the probed device, unlock fatally errors.

**`README.md`** — Pool unlock section (line 283): replace the stale paragraph:

> When unlocking on a fresh system (e.g., after migrating disks to a new machine), `unlock` automatically rebuilds the disk identity map from live pool state. Each disk's on-disk LUKS label is verified before recording.

This contradicts CLI-owned membership (unlock never creates or rebuilds pool.json) and the new UUID enforcement. Replace it with a paragraph covering: unlock verifies each disk's LUKS UUID against pool.json when metadata is present; mismatch is a fatal error. For fresh systems without pool.json, use `braid discover --write` or `braid add`.

### 4. Verify

Run `just test-rust` to confirm both new tests pass and all existing tests still pass.

Existing tests all use `DiskMember::from_by_id()` which has `luks_uuid: None`, so the check is skipped — no existing tests break.
