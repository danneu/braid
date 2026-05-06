# Auto-unlock keyfile lifecycle: audit + small hardening pass

## Context

`braid enroll DIR --generate` and `braid-auto-unlock.service` already implement
the right shape for the "USB-as-key" workflow: the generated keyfile lives
only on a mounted USB stick, and the service mounts that stick read-only,
reads the key once, and unmounts. The recent enrollment work
(`enroll_key_file.rs`, `storage.nix`) is good. But three gaps remain:

1. The auto-unlock cleanup (`umount /run/braid-key`) lives at the end of the
   happy path, with copies inside two refusal branches. Any future
   early-return that forgets to call it leaves the USB mounted with a
   plaintext key on it -- the exact pattern of the Unraid CVE the comment on
   `storage.nix:273` warns about.
2. The mount happens directly on `/run/braid-key`. Tmpfiles creates the
   underlying directory `0700 root:root`, but once mounted, the visible
   permissions of that path come from the USB filesystem's root inode --
   typically `0755` on vfat. Non-root users can therefore traverse into the
   mounted FS during the mount window. The 0700 only protects when nothing
   is mounted, which is the inverse of when protection matters.
3. The lifecycle invariants are scattered across code and not pinned by
   tests for the most security-sensitive cases (parent-dir mode,
   missing-file, symlink keyfile).

This is a small audit/hardening pass: pin the invariants in writing, fix
the mount-window exposure by mounting at a child path under a locked
parent, install an EXIT trap so the umount can never be skipped, add VM
tests for the cases not yet covered, and clean up the few doc examples
that still nudge users toward persistent host-side keyfile storage. No new
abstractions, no new commands.

## Lifecycle invariants (the contract this plan locks in)

1. The generated keyfile is only created when the target directory is an
   active mount point (`enroll_key_file.rs:572-579`).
2. The generated file is never overwritten (`create_new(true)` at
   `enroll_key_file.rs:333`).
3. No keyfile is created until passphrase verification and slot-1
   availability preflight have passed (`enroll_key_file.rs` orders preflight
   before line 486's `generate_key_file` call).
4. If the late `cryptsetup luksAddKey` step fails after the keyfile is
   created, the key is left on the USB so the user can retry -- no cleanup
   that would silently delete the user's only generated key.
5. Auto-unlock mounts the USB read-only (`ro,nosuid,nodev,noexec,nofail,
   noauto`), reads the key once, and unmounts.
6. **The USB is reachable only via a parent directory that root owns at
   `0700`.** The mount point is a child path (`/run/braid-key/mnt`) under
   `/run/braid-key`, so non-root cannot traverse to the mounted USB
   regardless of the USB filesystem's root inode permissions.
7. Symlink escape from the USB is refused: `realpath -e` resolves the
   keyfile and the resolved path must remain under the USB mount root
   (`/run/braid-key/mnt/`).

## Code change

### `modules/braid/storage.nix`

Three coordinated edits.

**1. Add a child tmpfiles entry for the mountpoint, keep the parent 0700:**

```nix
systemd.tmpfiles.rules = [
  "d /var/lib/braid 0750 root root -"
  "d ${cfg.mountPoint} 0755 root root -"
]
++ lib.optionals cfg.autoUnlock.enable [
  # Locked parent -- non-root cannot traverse into the mounted USB.
  "d /run/braid-key 0700 root root -"
  # Mount point for the USB. Permissions of this dir itself are
  # irrelevant once the USB is mounted on top, but the parent's 0700
  # blocks all non-root traversal.
  "d /run/braid-key/mnt 0700 root root -"
];
```

**2. Move the mount one level deeper:**

```nix
fileSystems."/run/braid-key/mnt" = lib.mkIf cfg.autoUnlock.enable {
  device = cfg.autoUnlock.keyDevice;
  fsType = "auto";
  options = [ "ro" "nosuid" "nodev" "noexec" "nofail" "noauto"
              "x-systemd.device-timeout=${toString cfg.autoUnlock.timeoutSec}s" ];
};
```

The corresponding systemd mount unit name is `run-braid\x2dkey-mnt.mount`.

**3. Update the script body (`storage.nix:212-277`).** The new shape:

```bash
# Cleanup is idempotent and must run on every exit path. Install the
# trap before the mount attempt so a partial/late mount from a failed
# `systemctl start` is still cleaned up.
trap 'umount /run/braid-key/mnt 2>/dev/null || true' EXIT

if ! ${pkgs.systemd}/bin/systemctl start run-braid\\x2dkey-mnt.mount 2>/dev/null; then
  echo "braid-auto-unlock: USB key device not available, skipping" >&2
  exit 0
fi

# keyPath is now /run/braid-key/mnt/braid.key. realpath -e fails on
# missing files (subsumes existence check); the case-prefix check
# refuses any symlink that escapes the USB mount root.
resolved=$(${pkgs.coreutils}/bin/realpath -e "${keyPath}" 2>/dev/null) || {
  echo "braid-auto-unlock: keyfile not found at ${keyPath}, skipping" >&2
  exit 0
}
case "$resolved" in
  /run/braid-key/mnt/*) ;;
  *)
    echo "braid-auto-unlock: keyfile resolves outside mount root ($resolved), refusing" >&2
    exit 0
    ;;
esac

# ...perms warning unchanged...
# ...braid unlock invocation unchanged...
# (Final inline `umount /run/braid-key 2>/dev/null || true` deleted --
#  the EXIT trap covers it.)
```

`keyPath` becomes `/run/braid-key/mnt/braid.key`. All three inline
`umount` calls (`storage.nix:237, 245, 275`) are deleted -- the trap is
the single cleanup mechanism. The "Unraid CVE" rationale comment moves
next to the trap.

## Tests

All new tests follow the project preamble convention (Intent / Why it
exists / Scenario) and are registered in `flake.nix` `checksFor` alongside
the existing `auto-unlock-*` checks.

The existing tests (`auto-unlock-key-present`, `auto-unlock-key-missing`,
`auto-unlock-key-wrong`) need three coordinated edits each, not just a
fixture path bump:

1. **Re-declare the mount one level deeper.** Each `.nix` file overrides
   the module's mount via `virtualisation.fileSystems."/run/braid-key"`
   (e.g. `auto-unlock-key-present.nix:95`). NixOS test infra uses
   `mkVMOverride` to replace module `fileSystems`, so each test must
   re-declare the new path: change the key from
   `"/run/braid-key"` to `"/run/braid-key/mnt"`. The options block
   (`ro,nosuid,nodev,noexec,nofail,noauto,x-systemd.device-timeout=10s`)
   is unchanged.
2. **Bump the keyfile path** wherever fixtures or shell commands write
   the key into the USB image. The on-USB filename remains `braid.key`;
   only the host-side path the test inspects changes from
   `/run/braid-key/braid.key` to `/run/braid-key/mnt/braid.key`.
3. **Bump mountpoint assertions** in each `.py` from
   `mountpoint -q /run/braid-key` to `mountpoint -q /run/braid-key/mnt`
   (e.g. `auto-unlock-key-present.py:39`).

### New: `tests/module/auto-unlock-runtime-dir-mode.nix` + `.py`

- **Intent:** `/run/braid-key` is `0700 root:root` -- and stays so even
  while the USB is mounted at `/run/braid-key/mnt` -- so non-root cannot
  traverse to the USB during the mount window.
- **Why it exists:** Pins invariant #6. The bug this guards against is
  exactly the one the original plan (pre-pivot) would have shipped:
  trusting the underlying tmpfiles dir's mode while a permissive USB
  filesystem was mounted on top of it. The mount window is the regime
  that matters; testing post-unmount would only re-verify tmpfiles.
- **Scenario:** Boot with `autoUnlock.enable = true` and a USB attached
  whose filesystem root is intentionally permissive (vfat formatted, no
  Unix perms). To exercise the mount-active state deterministically, the
  test does **not** rely on `braid-auto-unlock.service` (which would
  unmount via the EXIT trap before the test could observe). Instead, the
  test starts the mount unit directly:

  1. `systemctl start run-braid\x2dkey-mnt.mount` and verify
     `mountpoint -q /run/braid-key/mnt` succeeds.
  2. `stat -c '%a %U %G' /run/braid-key` returns `700 root root`.
  3. As non-root: `runuser -u nobody -- ls /run/braid-key` and
     `runuser -u nobody -- ls /run/braid-key/mnt` both fail with
     EACCES, even though the USB filesystem root is `0755`.
  4. `systemctl stop run-braid\x2dkey-mnt.mount` to clean up.

  The `braid-auto-unlock.service` itself can be masked or simply not
  exercised in this test; the focus is the directory-mode invariant
  during the mount window.

### New: `tests/module/auto-unlock-key-file-missing.nix` + `.py`

- **Intent:** USB is mounted but `braid.key` does not exist. Service
  skips, pool stays locked, USB is unmounted by the EXIT trap.
- **Why it exists:** Pins the `realpath -e` "no file" path. Distinct
  from the existing `auto-unlock-key-missing` test, which models
  USB-not-attached. The scenario this covers is "user wiped the wrong
  file" -- a real recovery story.
- **Scenario:** Configure `autoUnlock.keyDevice` to a virtual disk
  formatted with a filesystem but no `braid.key` at the root. Boot. Wait
  for `braid-auto-unlock.service` to settle. Assert: pool still locked,
  `/run/braid-key/mnt` is not a mount point (`mountpoint -q` exits
  non-zero), service unit succeeded.

### New: `tests/module/auto-unlock-key-file-symlink.nix` + `.py`

- **Intent:** USB present with `braid.key` as a symlink pointing outside
  the mount root (e.g. `-> /etc/shadow`). Service refuses, pool stays
  locked, USB is unmounted.
- **Why it exists:** Pins invariant #7. The path-traversal defense
  has no test today and is the highest-severity guard in the script.
- **Scenario:** Pre-populate the USB image so `braid.key` is a symlink
  to `/etc/shadow`. Boot. Assert: service log contains `keyfile resolves
  outside mount root`, pool stays locked, `/run/braid-key/mnt` is not a
  mount point, service exit 0.

### Skip (out of scope)

The "permissive perms warning still unmounts" case is covered implicitly
by the EXIT trap once the inline umounts are deleted -- if the trap
didn't run, none of the new tests would unmount either. A dedicated 0644
test would re-verify the trap; defer unless we hit a real regression.

## Documentation

### `docs/luks-unlock.md`

Four edits in this file:

- **Line 45 (slot tense):** "...a future binary keyfile would use a
  different slot." -> "...the binary keyfile uses slot 1."
- **Line 66 (auto-unlock keypath):** the bullet
  "`braid.autoUnlock` reading `/run/braid-key/braid.key`" needs the new
  path `/run/braid-key/mnt/braid.key`.
- **Lines 84-86 (Restricted mount point):** today this says
  "`/run/braid-key` is created with mode 0700 root:root, so even during
  the brief mount window, non-root users cannot traverse to the
  passphrase file." That sentence is the false invariant the pivot is
  fixing. Rewrite to explain the locked-parent design: the USB is
  mounted at `/run/braid-key/mnt`, and the *parent* `/run/braid-key`
  stays `0700 root:root`. Non-root cannot traverse the parent
  regardless of the USB filesystem's root inode permissions, so the
  mount-window exposure is closed.
- **Lines 161-163 (Mount point permissions section):** "since braid
  mounts the USB read-only at `/run/braid-key`..." needs the new
  path and a corrected explanation. The current text claims the mount
  point directory is "locked down so non-root users cannot reach the
  files," which is true only with the new layout. Update both the
  path and the rationale to match the locked-parent design.

### `manual/commands/unlock.md:33`, `add.md:56`, `replace.md:61`

These three pages still show `/etc/braid/keys` as a keyfile location:

- `unlock.md:33`: `sudo braid unlock --key-file /etc/braid/keys/braid.key`
- `add.md:56`: `sudo braid add ... --enroll /etc/braid/keys`
- `replace.md:61`: `sudo braid replace ... --enroll /etc/braid/keys`

That's a persistent host path adjacent to auto-unlock guidance and works
against the lifecycle this plan is locking in (key only ever lives on a
USB). Replace each example with a mounted USB path, e.g. `/mnt/usb`, and
where prose accompanies the example, add a one-liner that the directory
must be a mount point (the `--generate` error message already says so;
this is just propagating the guidance into the per-command pages).

### Audit of other docs (no change needed)

- `manual/guides/auto-unlock.md`: examples already use `/mnt/usb`.
- `README.md`: no `/etc/braid/keys` patterns in auto-unlock examples.
- `modules/braid/options.nix`: option descriptions are correct.
- `enroll_key_file.rs:577` error message already says
  "mount the USB device there before running braid enroll --generate".

## Files to modify

- `modules/braid/storage.nix` -- new tmpfiles rule, fileSystems path,
  trap insertion, inline-umount removal, mount-unit name update, keyPath
  bump, prefix bump.
- `tests/module/auto-unlock-key-present.{nix,py}` -- in `.nix`,
  re-declare `virtualisation.fileSystems."/run/braid-key/mnt"` (in
  place of the current `"/run/braid-key"` block); in `.py`, change
  `mountpoint -q /run/braid-key` to `mountpoint -q /run/braid-key/mnt`
  and bump any host-side keyfile paths.
- `tests/module/auto-unlock-key-missing.{nix,py}` -- same shape of
  edits where the path is referenced.
- `tests/module/auto-unlock-key-wrong.{nix,py}` -- same shape of
  edits where the path is referenced.
- `tests/module/auto-unlock-runtime-dir-mode.{nix,py}` (new).
- `tests/module/auto-unlock-key-file-missing.{nix,py}` (new).
- `tests/module/auto-unlock-key-file-symlink.{nix,py}` (new).
- `flake.nix` -- register the three new tests in `checksFor`.
- `docs/luks-unlock.md` -- slot-1 tense fix plus three path/wording
  updates for the locked-parent layout.
- `manual/commands/unlock.md`, `manual/commands/add.md`,
  `manual/commands/replace.md` -- replace `/etc/braid/keys` examples
  with mounted USB paths.

No Rust changes. The `enroll_key_file.rs` audit confirmed every invariant
already holds.

## Verification

```sh
just test-rust
just test-vm braid-enroll-generate \
            auto-unlock-key-present auto-unlock-key-missing \
            auto-unlock-key-wrong \
            auto-unlock-runtime-dir-mode \
            auto-unlock-key-file-missing \
            auto-unlock-key-file-symlink
```

The pre-existing four tests must still pass after the path move and trap
refactor; the three new tests pin the remaining invariants.

Manual sanity checks in a VM after the change:

- `sudo systemctl start braid-auto-unlock`, then `mountpoint -q
  /run/braid-key/mnt` returns non-zero within a second of the service
  exiting -- same observable behavior as today.
- As a non-root user (e.g. `nobody`), `ls /run/braid-key` and
  `ls /run/braid-key/mnt` both fail with `Permission denied` whether or
  not the USB is mounted.
