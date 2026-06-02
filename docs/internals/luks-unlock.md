---
intent: "Record LUKS Unlock Research Notes context for braid maintainers. Read before changing related behavior or docs."
status: Active
---
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

braid enrolls and opens **both** the shared passphrase and the auto-unlock
keyfile as LUKS *keyslot* secrets, so cryptsetup stretches both through the
keyslot KDF (Argon2id by default for LUKS2). Neither is a raw dm-crypt volume
key. The two differ in transport, byte handling, and which slot they occupy --
not in whether a KDF runs.

- **Passphrase** (slot 0): braid trims a trailing newline and rejects embedded
  line breaks (`cli/src/luks.rs#finalize_passphrase_bytes`), then pipes the
  bytes to cryptsetup via `--key-file=-` with no `--keyfile-size` (a passphrase
  is variable-length). Designed to protect a low-entropy human-chosen secret.

- **Binary keyfile** (slot 1): exactly 4096 bytes read via `--keyfile-size
  4096`, with no newline trimming. braid enforces the exact size before handing
  the path to cryptsetup (`cli/src/luks.rs#validate_user_keyfile_path`). High
  entropy, but still a KDF-protected keyslot secret -- not a raw key.

The passphrase and the keyfile are never interchangeable -- not even
byte-for-byte identical inputs -- for a fundamental reason: each LUKS keyslot
carries its own salt, so slot 0 and slot 1 derive different keys from identical
KDF input. Secondarily, at the cryptsetup level the bytes that reach the KDF
can also differ: a passphrase file containing `hunter2\n` feeds `hunter2` (the
trailing newline is trimmed) while a keyfile of the same bytes feeds `hunter2\n`
verbatim. That byte example is illustrative only -- braid's keyfile is always
exactly 4096 random bytes (anything else is rejected by
`validate_user_keyfile_path`), so the literal "same bytes" case never arises in
practice. The claim to reject is that one path skips a KDF; both run it.

A genuinely raw dm-crypt volume key would require `--volume-key-file`, which
braid forbids: it is in the `MANAGED_LUKS_FORMAT_LONG_FLAGS` denylist
(`cli/src/types.rs`), so braid refuses to let it reach `luksFormat`. The
passphrase-vs-keyfile `--keyfile-size` argv asymmetry is pinned by the block
comment above the test
`cli/src/cmd.rs#cryptsetup_luks_open_omits_keyfile_size`.

LUKS2 provides up to 32 keyslots per device; braid uses slot 0 for the
passphrase and slot 1 for the keyfile.

See: [cryptsetup(8) -- key-file processing](https://man7.org/linux/man-pages/man8/cryptsetup.8.html)
(the man page's "passed directly in dm-crypt" / no-digest note is scoped to the
*plain* device type, not LUKS),
[Arch Wiki -- dm-crypt/Device encryption](https://wiki.archlinux.org/title/Dm-crypt/Device_encryption)

## Keyfile creation target invariant

Any braid command path that creates or overwrites `braid.key` in a
user-supplied directory must verify that directory exists, is a
directory, and is an active mount point both at plan time and again
immediately before writing `braid.key`. The plan-time check alone is
insufficient: the seconds-long window between planning and the actual
write (passphrase prompt, Argon2 `--test-passphrase` verify against
every pool disk, per-disk `luksDump` slot inventory) lets a USB device
be unmounted (manual `umount`, hot-unplug, `systemd-automount` idle
timeout) after the gate passes, which would otherwise let the keyfile
land on the host root filesystem.

This currently applies to `braid enroll DIR --generate`. Existing-keyfile
consumers may read from ordinary admin-controlled paths and must not require a
mount point:

- `braid enroll DIR` without `--generate`
- `braid add --enroll DIR`
- `braid replace --enroll DIR`
- `braid unlock --key-file PATH`
- `braid.autoUnlock` reading `/run/braid-key/mnt/braid.key`

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
3. **Restricted mount root.** The USB is mounted at `/run/braid-key/mnt`,
   under a parent directory `/run/braid-key` that remains 0700 root:root.
   Non-root users cannot traverse the parent regardless of the USB
   filesystem's root inode permissions, so the passphrase file stays
   unreachable during the mount window.

## Credential memory hygiene

Passphrase buffers in the CLI are `Zeroizing<...>` from read to drop
(`cli/src/luks.rs::read_line_into_zeroizing`,
`cli/src/luks.rs::read_file_into_zeroizing`), and subprocess delivery is
stdin-only with no argv argument or temporary file. Generated keyfile bytes
are zeroized after write (`cli/src/enroll_key_file.rs::generate_key_file`).
Passphrases and keyfile bytes never enter the Nix store; the upsmon token is
generated at runtime per [decision 020](../design/decisions/020-ups-integration.md),
and the USB keyfile lives only on the USB stick mounted into
`/run/braid-key/mnt/` as hardened in commit `df706c44875f`.

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

## Open-failure header diagnosis

Unlock is two-phase. `plan_open_pool` probes every declared disk and
classifies it (`ConfigDiskState`); the disks it hands to
`execute_unlock_and_mount` as `to_unlock` are exactly the ones it found
`PresentLuks` -- header intact, both `luksUuid` and `luksDump` succeeded at
plan time. `execute_unlock_and_mount` then verifies the credential and opens
each disk.

When verify or open fails, `open_disks_with_credential` re-probes the header
at failure time and routes the result through `explain_open_failure`:

- `Unreadable` -- emit the off-system-backup guidance (per the messaging
  invariant above).
- `Ok` -- the header is intact, so the original cryptsetup/verify error is
  passed through verbatim (e.g. a genuine wrong passphrase).
- `ProbeFailed` -- the probe itself could not run, so braid reports that
  diagnosis is incomplete rather than guessing a cause.

The failure-time re-probe is deliberate, not redundant. Because the
`to_unlock` disks were `PresentLuks` by construction, the planner holds no
header-damage observation to thread in -- there is nothing to reuse. The
header can still change in the plan->open window (external `dd`, a hardware
fault, a swapped device), and the failure-time probe is exactly what keeps a
wiped or damaged header from being misdiagnosed as a "wrong passphrase".

`probe_luks_header` -> `LuksHeaderState` is the single header-damage
classifier; `ConfigDiskState` is a separate, coarse membership gateway, so the
two neither duplicate nor drift.

## Unparseable state-file reconciliation

There are two state files that can block normal operation when they are
unparseable: `/var/lib/braid/pool.json` and
`/var/lib/braid/pending-op.json`.

For a **corrupt or off-schema** `pool.json`, the remediation phrase is:

`run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/internals/luks-unlock.md)`

Confirm the attached disks are the intended pool members, then run
`braid discover --write` -- the corrupt file is overwritten in place and the
original bytes are preserved at `pool.json.corrupt-<RFC3339-UTC>` next to it.
The snapshot is a hard precondition for the rebuild: if it cannot be written
(full disk, read-only state directory), `discover --write` refuses with
`failed to snapshot existing corrupt file to ...` so the corrupt original is
not destroyed; free disk space or fix permissions and retry. The sidecar is
safe to remove once you have manually copied any still-relevant prior-binding
bytes (e.g. `devid` for a `null_underlying` member). If you know the expected
member count ahead of time, pass
`--expect-count <N>` to fail closed against a temporarily detached disk or an
unrelated braid-labeled disk being silently admitted.

Note: `braid lock` -- the user-facing command, the `braid-online.service`
ExecStop path, and `braid lock --dry-run` alike -- does NOT fail under a
missing or corrupt `pool.json`. It warns and proceeds with empty membership;
every observed `braid-*` mapper is then verified by its backing LUKS UUID
before close, so shutdown cleanup stays complete. No lock pathway hard-fails
on an unloadable `pool.json`.

For a **healthy UUID-keyed** `pool.json`, do not run `discover --write` at all
-- use `braid add` / `braid remove` / `braid replace` to mutate membership.
`discover --write` is a repair tool, not a refresh; running it against a
healthy file refuses (`is already a healthy UUID-keyed membership`) so it does
not drop persisted devid bindings (decision 024).

For an unparseable pending-operation journal, the remediation phrase is:

`Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run.`

It is safe to remove `pending-op.json` only when one of these is true:

- The operation has not yet committed any disk-level mutation: no LUKS format
  was applied, no `btrfs device add` ran, and no fresh-format target was opened.
- The user has confirmed with `braid status` that the live pool already
  reflects the intended state and the journal entry is stale.

It is not safe to remove `pending-op.json` when a partially completed mutation
is in flight, such as `mkfs.btrfs` succeeding but `btrfs device add` not yet
running, or a `replace` paused mid-rebuild. In those cases, follow the
[recovery scenario guide](../guides/recovery-scenarios.md) instead.

## Replace Target Size Preflight

`braid replace` mirrors btrfs's own source-size authority by issuing
`BTRFS_IOC_DEV_INFO` for the source devid and reading `total_bytes`, the same
value `btrfs replace start` compares against. The ioctl is wrapped behind the
`BtrfsDevInfo` trait so planning code can be unit-tested like the existing
`Filesystem` boundary; production uses `LinuxBtrfsDevInfo` with
`nix::ioctl_readwrite!`.

Target capacity is computed before opening the replacement mapper. Existing
LUKS targets read LUKS2 segment `offset` and `size` from
`cryptsetup luksDump --dump-json-metadata`: `dynamic` segments use
`raw - offset` with no sector_size rounding because cryptsetup sizes the
dm-crypt device that way exactly and the kernel rejects, rather than rounds, a
non-sector_size-multiple mapper, so an existing container's capacity is exact at
any sector_size. Fixed segments use `segment.size` directly. Fresh targets
instead assume cryptsetup's default 16 MiB LUKS2 offset, which holds because
braid rejects `--sector-size` and offset-changing format flags. If any of those
values cannot be read or parsed, or the computed target capacity is smaller
than the source `total_bytes`, replace refuses before writing `pending-op.json`,
formatting a fresh target, or opening the replacement mapper.

## Failed unlock cleanup

If `braid unlock` or a recovery mount path opens one or more LUKS mappers
but fails before mounting the pool, braid fails closed for only the mappers
opened by that command invocation.

Cleanup is scoped by the LUKS open helper's ownership result:

- `Opened`: braid created the mapper during this command and may close it on
  failure.
- `AlreadyOwned`: the mapper was already open at execution time, including
  races where an operator opened it after planning. braid must not close it.

The cleanup sequence is:

1. If any opened mapper path still exists under `/dev/mapper`, run scoped
   `btrfs device scan --forget <paths>` for those paths. Failure warns and
   cleanup continues.
2. Close every opened mapper with the same retry-on-busy behavior as
   `braid lock`.

When no mapper was opened, cleanup is a silent no-op: there is no
`btrfs device scan --forget`, no `cryptsetup close`, and no trailing cleanup
summary. This is the expected wrong-passphrase shape.

After attempting non-empty cleanup, stderr includes one trailing summary line:

- Success: `cleanup: closed LUKS mappers opened by this command.`
- Failure: `cleanup failed: one or more LUKS mappers opened by this command could not be closed; run 'braid lock' after resolving the issue. First cleanup error: ...`

The original unlock or mount error remains the command's primary error;
cleanup output is secondary guidance and never replaces it.

## Mount point permissions

Standard guidance for directories containing LUKS key material: the
directory should be mode 0700 owned by root, and keyfiles should be mode
0400. Since braid mounts the USB read-only at `/run/braid-key/mnt`, file
permissions are whatever the USB filesystem has -- but the locked parent
directory `/run/braid-key` prevents non-root users from traversing to the
mounted files.

See: [LUKS key file permissions](https://itsfoss.gitlab.io/post/luks-key-file-correct-permissions/)
