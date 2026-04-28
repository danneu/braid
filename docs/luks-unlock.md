# LUKS Unlock: Research Notes

Reference material for braid's unlock mechanisms. Covers gotchas, security
considerations, and design rationale discovered during implementation.

## USB device naming stability

`/dev/sdX` names are assigned by probe order and shift when devices are
added, removed, or enumerated differently across reboots. A USB stick that
was `/dev/sdd` can become `/dev/sdc` if another drive is unplugged.

`/dev/disk/by-id/` paths use hardware serial numbers reported by the device
firmware and are stable across reboots and topology changes. Always use
by-id for any persistent reference to a block device.

```
# Unstable — changes when drives are added/removed:
/dev/sdd

# Stable — tied to hardware serial, survives reboot and topology changes:
/dev/disk/by-id/usb-Kingston_DataTraveler_3.0_E0D55EA573FCF450-0:0
```

See: [Arch Wiki — Persistent block device naming](https://wiki.archlinux.org/title/Persistent_block_device_naming)

## Passphrase file vs binary keyfile

cryptsetup treats these as fundamentally different inputs:

- **Passphrase** (stdin or `--passphrase-file`): read until first newline,
  processed through PBKDF2 (LUKS1) or Argon2id (LUKS2) for key stretching.
  Designed to protect low-entropy human-chosen secrets.

- **Binary keyfile** (`--key-file`): raw bytes read up to the cipher key
  size, used directly as key material with no derivation. High entropy
  assumed.

These are not interchangeable even if they contain the same bytes. A
passphrase file containing `hunter2\n` and a binary keyfile containing the
same bytes will produce different LUKS decryption keys because the
passphrase path applies PBKDF while the keyfile path does not.

Each mechanism occupies a separate LUKS key slot (up to 8 slots per
device). Braid's shared passphrase uses slot 0; a future binary keyfile
would use a different slot.

See: [cryptsetup(8) — key-file processing](https://man7.org/linux/man-pages/man8/cryptsetup.8.html),
[Arch Wiki — dm-crypt/Device encryption](https://wiki.archlinux.org/title/Dm-crypt/Device_encryption)

## Plaintext keyfile exposure (Unraid CVE)

Unraid stores the LUKS passphrase in plaintext at `/root/keyfile` on
persistent storage. This means anyone with root access or physical access to
the boot drive can read the encryption passphrase — the encryption is
effectively defeated at rest.

See: [Unraid forum — LUKS password stored in plaintext at /root/keyfile](https://forums.unraid.net/topic/83022-luks-password-stored-in-plaintext-at-rootkeyfile/)

Braid avoids this in three ways:

1. **No local storage.** The passphrase file lives on a removable USB
   device, never copied to the host filesystem.
2. **Mount-read-unmount.** The auto-unlock service mounts the USB read-only,
   reads the passphrase, then unmounts immediately. The passphrase is not
   accessible on the filesystem after unlock completes.
3. **Restricted mount point.** `/run/braid-key` is created with mode 0700
   root:root, so even during the brief mount window, non-root users cannot
   traverse to the passphrase file.

## Boot resilience: nofail + device-timeout

The USB mount uses `nofail` and `x-systemd.device-timeout=Ns`. Together
these guarantee the USB device never blocks boot:

- `nofail`: systemd does not treat a failed mount as a boot failure.
- `x-systemd.device-timeout`: systemd waits at most N seconds for the
  block device to appear, then gives up.
- `noauto`: the mount is not started at boot; it is triggered on-demand
  by the automount unit when the auto-unlock service accesses the mount
  point.

If the USB stick is not plugged in, the automount times out, the
auto-unlock service sees no key file, logs an informational message, and
exits 0. Boot continues normally; the pool stays locked for manual unlock.

## Header backup workflow and messaging

LUKS header backups protect against on-disk header corruption. braid's `add`, `replace`, and `enroll_key_file` create local `.luksheader` files at `/var/lib/braid/luks-headers/<disk>.luksheader` as a transient byproduct -- they are **not** the intended backup target. The product workflow is:

1. braid writes a local `.luksheader` during a header-mutating operation.
2. The user exports the header off-system (USB, second machine, cloud key storage, etc.).
3. The user removes the local copy. `braid status` and the TUI warn while a local copy persists, because its continued presence on the same machine defeats the off-system backup model.

### Messaging invariant

User-facing recovery, restoration, and backup-status messages -- in `doctor`, `status`, `unlock` errors, the TUI, or any new command -- must NOT reference local `/var/lib/braid/luks-headers/*.luksheader` files. Recovery guidance is generic: "restore from your off-system LUKS header backup if you have one." Specifically:

- Never branch on whether a local `.luksheader` file exists.
- Never call `Path::exists` on `paths.luks_headers_dir().join(...)` to change user-visible advice.
- Never tell users to run `cryptsetup luksHeaderRestore --header-backup-file /var/lib/braid/...`.

If `doctor` pointed users at the local files, the product would be internally inconsistent: `status` and the TUI warn about the same artifact `doctor` would tell users to depend on. Generic guidance is the right answer even if the local backup happens to be present and would technically work.

Red flags when reviewing recovery messaging: `/var/lib/braid/luks-headers/`, `.luksheader`, `luks_headers_dir()`, and any `Path::exists` against a backup path.

## Mount point permissions

Standard guidance for directories containing LUKS key material: the
directory should be mode 0700 owned by root, and keyfiles should be mode
0400. Since braid mounts the USB read-only at `/run/braid-key`, file
permissions are whatever the USB filesystem has — but the mount point
directory itself is locked down so non-root users cannot reach the files.

See: [LUKS key file permissions](https://itsfoss.gitlab.io/post/luks-key-file-correct-permissions/)
