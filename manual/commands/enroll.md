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

`/mnt/usb` must already exist and be mounted. This creates `/mnt/usb/braid.key` (4096 bytes of random data) and adds it to LUKS slot 1 on every disk in the pool. You'll be prompted for the pool passphrase.

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
| `--generate` | Create a new 4096-byte random keyfile before enrolling; the target directory must already be a mount point |
| `--passphrase-stdin` | Read passphrase from stdin instead of TTY prompt |
| `--passphrase-file <path>` | Read passphrase from a file instead of TTY prompt (conflicts with `--passphrase-stdin`) |
| `--dry-run` | Show what would happen without making changes |

## What happens under the hood

1. Checks for a pending operation journal (refuses if one exists).
2. **With `--generate`:** Validates that the target directory exists, is a directory, is already a mount point, and does not already contain `braid.key` (if a prior `--generate` run was interrupted, drop `--generate` and re-run to finish enrolling the existing keyfile; otherwise remove it manually first).
3. **Without `--generate`:** Validates that `DIR/braid.key` already exists and is a regular file.
4. Scans pool membership for present LUKS disks. Absent or non-LUKS disks are skipped with a message.
5. Verifies the passphrase against every present pool disk before any keyfile probe.
6. **Without `--generate`:** Probes the keyfile against each disk. If it authenticates, reports "already enrolled" and skips that disk for the rest of enrollment. A rejected probe means the disk still needs enrollment; any other probe failure (e.g. device busy) aborts immediately rather than treating the disk as un-enrolled.
7. For each disk still needing enrollment, checks LUKS slot 1: proceeds if free; refuses with an error if occupied by an unknown key (you must remove it first with `cryptsetup luksKillSlot`).
8. **With `--generate`:** Only after all preflight checks pass, generates the random keyfile.
9. Enrolls the keyfile into LUKS slot 1 on each disk.
10. Creates a LUKS header backup for each modified disk.

## Safety checks

- Refuses if a pending operation exists (recovery mode).
- With `--generate`, refuses unless the target directory is already a mount point.
- Passphrase is verified before any mutations.
- Slot 1 conflicts are detected before the keyfile is generated, so you never end up with an orphan keyfile.
- With `--generate`, refuses if `braid.key` already exists at the target path; if a prior `--generate` run was interrupted, drop `--generate` and re-run to finish enrolling the existing keyfile.
- Without `--generate`, refuses if the keyfile doesn't exist.
- Idempotent: if the keyfile is already enrolled on a disk, that disk is skipped.

## Related commands

- [unlock](unlock.md) -- use `--key-file` to unlock with the enrolled keyfile

## Related guides

- [Auto-unlock](../guides/auto-unlock.md)
