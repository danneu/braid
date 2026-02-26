# Decision: Single Passphrase

Status: Active

> Principle: [Single passphrase](../principles.md#4-single-passphrase)

## Context

`braid unlock` and `braid add` prompt for a passphrase that unlocks all LUKS devices. If each drive had a different passphrase, the user would need to type N passphrases on every unlock. The UX must be: one passphrase, all drives unlock.

## Options considered

1. **Shared keyfile on boot disk** — store a keyfile on the SSD, encrypt the SSD with a passphrase. Unlocking the SSD exposes the keyfile, which unlocks data drives. More complex boot chain, keyfile is at-rest on disk.
2. **Same passphrase, no enforcement** — tell users to use the same passphrase. They'll forget or mistype. Boot breaks silently.
3. **Same passphrase, enforced at format time** — `braid-add-disk` verifies the passphrase matches existing pool members before formatting. Catches mismatches immediately.

## Decision

Option 3. Enforcement at format time with zero at-rest key material.

### How it works

- **First disk**: prompt for passphrase twice (confirm match). Standard new-passphrase flow.
- **Subsequent disks**: prompt once, then verify against an existing LUKS device in the pool via `cryptsetup luksOpen --test-passphrase`. If verification fails, refuse to proceed with a clear error.

### Finding a verification target

The script reads which devices are in the btrfs pool (`btrfs fi show /mnt/storage`), picks one that's currently open, and tests the passphrase against its underlying LUKS device.

## Constraint

No keyfiles. The passphrase is never written to disk. It exists only in memory during `braid-add-disk` execution and in the user's head.

## See

- `cli/src/` — passphrase prompt and verification logic in the Rust CLI
- `design-docs/1-braid-add-disk.md` — original script design (historical)
