[← Manual](../index.md)

# braid enroll

Enrolls a binary keyfile into LUKS slot 1 on all pool disks. Used to set up USB auto-unlock: plug in a USB drive with the keyfile, and `braid unlock --key-file` can open the pool without typing a passphrase.

## When to use it

- Setting up unattended unlock via USB keyfile.
- After adding a new disk to the pool (enroll the keyfile on it too).

## Basic example

Generate a new keyfile on a USB drive and enroll it on all pool disks:

```
sudo braid enroll /mnt/usb --generate
```

This creates `/mnt/usb/braid.key` (4096 bytes of random data) and adds it to LUKS slot 1 on every disk in the pool. You'll be prompted for the pool passphrase.

## Common variations

Enroll an existing keyfile (already at `/mnt/usb/braid.key`):

```
sudo braid enroll /mnt/usb
```

Non-interactive (passphrase from stdin):

```
echo -n 'my-passphrase' | sudo braid enroll /mnt/usb --generate --passphrase-stdin
```

Passphrase from a file:

```
sudo braid enroll /mnt/usb --generate --passphrase-file /root/passphrase.txt
```

Dry run (preview what would happen):

```
sudo braid enroll /mnt/usb --generate --dry-run
```

## Flags

| Flag | Effect |
| --- | --- |
| `--generate` | Create a new 4096-byte random keyfile before enrolling |
| `--passphrase-stdin` | Read passphrase from stdin instead of TTY prompt |
| `--passphrase-file <path>` | Read passphrase from a file instead of TTY prompt |
| `--dry-run` | Show what would happen without making changes |

## What happens under the hood

1. Checks for a pending operation journal (refuses if one exists).
2. Scans pool membership for present LUKS disks. Absent or non-LUKS disks are skipped with a message.
3. **With `--generate`:** Validates that `braid.key` does not already exist at the target path (refuses if it does -- remove it manually first).
4. Verifies the passphrase against the first pool disk.
5. For each disk, checks LUKS slot 1:
   - If the keyfile already works on this disk, reports "already enrolled" and skips.
   - If slot 1 is occupied by an unknown key, refuses with an error (you must manually remove it first with `cryptsetup luksKillSlot`).
   - If slot 1 is free, proceeds.
6. **With `--generate`:** Only after all preflight checks pass, generates the random keyfile.
7. Enrolls the keyfile into LUKS slot 1 on each disk.
8. Creates a LUKS header backup for each modified disk.

## Safety checks

- Refuses if a pending operation exists (recovery mode).
- Passphrase is verified before any mutations.
- Slot 1 conflicts are detected before the keyfile is generated, so you never end up with an orphan keyfile.
- With `--generate`, refuses if `braid.key` already exists at the target path.
- Without `--generate`, refuses if the keyfile doesn't exist.
- Idempotent: if the keyfile is already enrolled on a disk, that disk is skipped.

## Related commands

- [unlock](unlock.md) -- use `--key-file` to unlock with the enrolled keyfile

## Related guides

- [Auto-unlock](../guides/auto-unlock.md)
