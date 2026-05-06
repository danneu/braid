# Harden Mapper Ownership, Then Remove Redundant Bootstrap Btrfs Probes

## Summary

- Do not trust "fresh mapper" as an implicit bootstrap invariant.
- First make both LUKS open helpers prove that an already-open `/dev/mapper/braid-<name>` belongs to the requested by-id disk.
- After mapper ownership is enforced, simplify the bootstrap helpers into dumb `mkfs` + `mount` wrappers and remove their btrfs probe layer.

## Key Changes

- In `cli/src/luks.rs`, add a shared helper that returns whether `/dev/mapper/braid-<name>` is inactive or already owned by the requested by-id disk.
- Replace the `fs.exists("/dev/mapper/...") -> Ok(())` shortcuts in both `ensure_luks_open` and `ensure_luks_open_with_key_file` with that shared ownership helper.
- Change both open helpers to no longer require a `Filesystem` argument. Update callers in `add`, `mount`, `recover`, and `replace`.
- In the shared helper, run `CryptsetupStatus { mapper }`. If inactive, let the caller run the normal open command.
- If the mapper is active, read the requested by-id disk's LUKS UUID and the active backing device's LUKS UUID. Treat ownership as valid only when they match.
- Add `LuksError::MapperConflict { name, expected, found }`, mirroring the existing `ProbeError::MapperConflict` message shape. Use `found: None` for `device: (null)` and for an active backing device whose `CryptsetupLuksUuid` command reports "not a LUKS device".
- Add `LuksError::Parse(#[from] ParseError)` so malformed `cryptsetup status` output and malformed LUKS UUID output propagate as parser errors, not as mapper conflicts.
- In `cli/src/pool.rs`, delete `assert_no_btrfs_superblock` and its `btrfs_filesystem_show` parser imports.
- Make `pool_bootstrap_mount` run `MkfsBtrfs` unconditionally, then create the mount point and mount.
- Make `pool_bootstrap_mount_raid1` run `MkfsBtrfsRaid1` unconditionally, then create the mount point and mount.
- Keep `BtrfsFilesystemShowTarget` behavior in `add.rs` and `recover.rs`; those probes still belong to identity and recovery checks.

## Interfaces

- No CLI command, config, journal, or command enum changes.
- Internal Rust API change: `ensure_luks_open(runner, name, by_id, passphrase)` and `ensure_luks_open_with_key_file(runner, name, by_id, key_file_path)` drop their filesystem parameters.
- Internal error type change: `LuksError` gains `MapperConflict` and `Parse`.

## Test Plan

- Add `luks.rs` unit tests for the passphrase open path:
  - inactive mapper runs `CryptsetupLuksOpen`
  - active mapper with matching backing UUID returns `Ok(())` and does not open
  - active mapper with a different backing UUID returns `LuksError::MapperConflict` and does not open
  - active mapper with `device: (null)` returns `LuksError::MapperConflict` and does not open
  - active mapper backed by a non-LUKS device returns `LuksError::MapperConflict { found: None }` and does not open
- Add matching `luks.rs` unit tests for the keyfile open path:
  - active mapper with matching backing UUID returns `Ok(())` and does not run `CryptsetupLuksOpenKeyFile`
  - active mapper with a different backing UUID returns `LuksError::MapperConflict`
  - active mapper with `device: (null)` returns `LuksError::MapperConflict`
- Add `luks.rs` parser propagation tests:
  - malformed active `cryptsetup status` output returns `LuksError::Parse`, not `MapperConflict`
  - active mapper, valid requested by-id UUID, and active backing `CryptsetupLuksUuid` exit 0 with invalid UUID text returns `LuksError::Parse`, not `MapperConflict`
- Add a command-level fresh-add regression in `add.rs`: unmounted pool, empty membership, one `PresentNotLuks` disk, and a pre-existing `/dev/mapper/braid-disk1` backed by a different or non-LUKS device. Assert the command errors before any `MkfsBtrfs` or mount request.
- Add a command-level mixed RAID1 regression in `add.rs`: unmounted pool, empty membership, one `PresentNotLuks` disk and one braid-labeled `PresentLuks` disk. Assert the "bootstrap only accepts fresh disks" rejection, no journal, no inhibitor acquisition, and no `MkfsBtrfsRaid1` request.
- Update pool bootstrap unit tests so successful single-disk bootstrap expects exactly `MkfsBtrfs`, then `Mount`.
- Update pool bootstrap unit tests so successful RAID1 bootstrap expects exactly `MkfsBtrfsRaid1`, then `Mount`.
- Delete pool tests that expect bootstrap helpers to refuse existing superblocks or ambiguous probe errors.
- Update stale test comments that refer to hard-coded line numbers for the bootstrap rejection invariant.
- Run `just test-rust`.

## Assumptions

- The conflict-after-journal fresh-add regression should assert no filesystem formatting or mount. It does not need to assert no journal, because the current add flow writes the journal before irreversible fresh-disk execution.
- No VM test is required because the change is covered by Rust unit tests plus command-level `cmd_add` tests, and the cryptsetup status parser is already fixture-backed.
