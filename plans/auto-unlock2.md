# Plan: USB Random Keyfile Auto-Unlock

## Context

braid currently unlocks all LUKS pool disks with a single shared passphrase
(slot 0). Unlocking requires an interactive `braid unlock` session — someone
must SSH in and type the passphrase after every reboot.

This plan adds unattended auto-unlock via a binary random keyfile on a
removable USB device. The keyfile uses a separate LUKS key slot (slot 1) and
is a completely independent credential from the passphrase. Stealing the USB
does not reveal the interactive passphrase. The passphrase remains the
interactive-unlock mechanism; the keyfile is for auto-unlock only.

Prep work already done:
- `docs/luks-unlock.md` — research notes (USB naming, passphrase vs keyfile
  semantics, Unraid CVE, boot resilience, mount permissions)
- `docs/decisions/single-passphrase.md` — updated Scope section (keyfiles
  are orthogonal to the shared passphrase)

## Phase 1: CmdRequest variants + luks.rs helpers

Foundation — everything else builds on this.

### cmd.rs — three new CmdRequest variants

File: `cli/src/cmd.rs`

**CryptsetupLuksOpenKeyFile { device, mapper, key_file_path }**
- Implemented in `run()` (no stdin)
- Command: `cryptsetup open --type luks --key-file <path>
  --perf-no_read_workqueue --perf-no_write_workqueue <device> <mapper>`

**CryptsetupTestKeyFile { device, key_file_path }**
- Implemented in `run()` (no stdin)
- Command: `cryptsetup open --test-passphrase --key-file <path> <device>`

**CryptsetupLuksAddKeyFile { device, key_file_path }**
- Implemented in `run_with_stdin()` — existing passphrase on stdin authorizes
  the new key enrollment
- Command: `cryptsetup luksAddKey --key-slot 1 <device> <key_file_path>`
  (stdin = passphrase)
- `--key-slot 1` ensures deterministic slot assignment: slot 0 = passphrase,
  slot 1 = keyfile. Without explicit assignment, cryptsetup auto-picks the
  next free slot, which makes slot layout unpredictable if a previous
  enrollment left debris. Explicit slots simplify auditing (`cryptsetup
  luksDump`) and future revocation.
- Add `run()` guard (must use `run_with_stdin`, same pattern as
  CryptsetupLuksOpen at cmd.rs:263-268)

Note on `luksAddKey` semantics: stdin is the default source for the
*existing* credential. The positional arg is the *new* keyfile to enroll.
No `--key-file=-` flag needed — that would change the existing key source.

Slot convention (document in luks.rs as constants):
```rust
pub const LUKS_SLOT_PASSPHRASE: u8 = 0;
pub const LUKS_SLOT_KEYFILE: u8 = 1;
```

### luks.rs — three new functions

File: `cli/src/luks.rs`

```
pub fn ensure_luks_open_with_key_file(runner, fs, key, disk, key_file_path) -> Result<(), LuksError>
```
Mirrors `ensure_luks_open()` (line 125) but uses CryptsetupLuksOpenKeyFile
via `runner.run()`.

```
pub fn verify_key_file(runner, device, key_file_path) -> Result<bool, LuksError>
```
Mirrors `verify_passphrase()` (line 110) but uses CryptsetupTestKeyFile.

```
pub fn enroll_key_file(runner, device, passphrase, key_file_path) -> Result<(), LuksError>
```
Uses CryptsetupLuksAddKeyFile via `runner.run_with_stdin()` with passphrase
bytes as stdin.

## Phase 2: `braid unlock --key-file`

Depends on: Phase 1.

### main.rs — new flag on UnlockArgs

File: `cli/src/main.rs` (line 119-127)

Add to UnlockArgs:
```rust
#[arg(long, conflicts_with_all = ["passphrase_stdin", "passphrase_file"])]
key_file: Option<std::path::PathBuf>,
```

Pass `args.key_file.as_deref()` through to `cmd_unlock`.

### unlock.rs — keyfile branch

File: `cli/src/unlock.rs` (line 29-35, 90-109)

Add `key_file: Option<&std::path::Path>` parameter to `cmd_unlock`.

At step 4 (line 90-109), branch:
- If `key_file` is Some: verify against first disk with
  `luks::verify_key_file()`, then open each disk with
  `luks::ensure_luks_open_with_key_file()`.
- If `key_file` is None: existing passphrase path (unchanged).

Steps 1-3 (mountpoint check, probe, early exit) and 5-6 (btrfs scan, mount)
are shared — no changes.

## Phase 3: `braid enroll-key-file` command

Depends on: Phase 1.

### New module: cli/src/enroll_key_file.rs

```rust
pub fn cmd_enroll_key_file(runner, fs, config, key_file_path,
    passphrase_stdin, passphrase_file) -> Result<(), EnrollKeyFileError>
```

Steps:
1. Validate keyfile exists and is a regular file.
2. Read passphrase via `luks::read_passphrase()` (to authorize enrollment).
3. Verify passphrase against first present LUKS disk.
4. For each config disk that is present + LUKS-formatted:
   a. `luks::verify_key_file()` — if already enrolled, skip (idempotent).
   b. Otherwise `luks::enroll_key_file()` to add to new slot.
   c. Print per-disk result.
5. Print summary.

### main.rs — register subcommand

File: `cli/src/main.rs`

Add `EnrollKeyFile(EnrollKeyFileArgs)` to Commands enum (line 22-41).

```rust
struct EnrollKeyFileArgs {
    path: std::path::PathBuf,
    #[arg(long)]
    passphrase_stdin: bool,
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
}
```

### lib.rs — export module

File: `cli/src/lib.rs` — add `pub mod enroll_key_file;`

## Phase 4: `braid add --enroll-key-file`

Depends on: Phase 3.

### main.rs — new flag on AddArgs and ReplaceArgs only

File: `cli/src/main.rs`

Add to AddArgs (line 64-70) and ReplaceArgs (line 91-103):
```rust
/// Also enroll this binary keyfile in the new disk (LUKS slot 1)
#[arg(long)]
enroll_key_file: Option<std::path::PathBuf>,
```

NOT on CommonArgs — `--enroll-key-file` is meaningless on `remove` and
`remove-missing`. Putting it on CommonArgs leaks the flag into destructive
commands where it would be silently ignored, creating intent ambiguity.

### add.rs — enroll after format

File: `cli/src/add.rs`

After `luks_format()` + `ensure_luks_open()` (the PresentNotLuks branch),
if `enroll_key_file` is Some, call `luks::enroll_key_file()` with the
passphrase already in scope.

Same change in `cli/src/replace.rs` for the replace-formats-new-disk path.

## Phase 5: NixOS module

Depends on: Phase 2 (`--key-file` flag must exist).

### options.nix — new option subtree + assertions

File: `modules/braid/options.nix`

```nix
autoUnlock = {
  enable = lib.mkEnableOption "USB keyfile auto-unlock for braid pool";

  # keyDevice must use /dev/disk/by-id/ — /dev/sdX names shift when devices
  # are added or removed. by-id paths use hardware serial numbers and are
  # stable across reboots. See docs/luks-unlock.md § "USB device naming
  # stability".
  keyDevice = lib.mkOption {
    type = lib.types.str;
    description = "Block device for the USB key (/dev/disk/by-id/...).";
  };

  # This is a binary random keyfile, NOT a passphrase file. cryptsetup
  # treats these differently: keyfiles are used as raw key material (no
  # PBKDF), while passphrases go through Argon2id/PBKDF2 key stretching.
  # They occupy separate LUKS key slots and are not interchangeable.
  # See docs/luks-unlock.md § "Passphrase file vs binary keyfile".
  #
  # This path is relative to the USB mount root (/run/braid-key). No
  # leading slash, no ".." components. Resolved via safe path-join under
  # the fixed root to prevent path traversal (CWE-22).
  keyFile = lib.mkOption {
    type = lib.types.str;
    default = "braid.key";
    description = "Relative path to keyfile within the mounted USB filesystem.";
  };

  timeoutSec = lib.mkOption {
    type = lib.types.ints.positive;
    default = 5;
    description = "Seconds to wait for USB device before giving up.";
  };
};
```

Assertions:
- `cfg.package != null` when `braid.enable = true` (the refactored
  braid-unlock service calls the CLI binary; making this explicit prevents
  a broken service if someone enables the module without the package)
- `cfg.autoUnlock.enable -> hasPrefix "/dev/disk/by-id/" cfg.autoUnlock.keyDevice`
- `cfg.autoUnlock.enable -> cfg.autoUnlock.timeoutSec > 0`
- keyFile path safety (CWE-22): reject if it starts with `/` or contains `..`.
  This is the config-time half of path traversal defense — catches obvious
  misconfiguration at `nixos-rebuild` time. The runtime half (symlink
  rejection in the service script) handles attacks via USB filesystem content.

  ```nix
  {
    assertion = cfg.autoUnlock.enable ->
      !(lib.hasPrefix "/" cfg.autoUnlock.keyFile)
      && !(lib.hasInfix ".." cfg.autoUnlock.keyFile);
    message = "braid.autoUnlock.keyFile must be a relative path with no '..' components.";
  }
  ```

### storage.nix — add auto-unlock, refactor manual service

File: `modules/braid/storage.nix`

**1. Refactor braid-unlock service** (line 94-142) to call CLI:
```nix
path = [ cfg.package cfg.packages.cryptsetup cfg.packages.btrfsProgs cfg.packages.utilLinux ];
script = ''
  ${pkgs.systemd}/bin/systemd-ask-password \
    --id=braid "LUKS passphrase for braid pool:" \
  | braid unlock --passphrase-stdin
'';
```
`cfg.package` is the unwrapped CLI binary — it shells out to `cryptsetup`,
`btrfs`, and `mount` internally, so all tool packages must be in PATH.

This eliminates the duplicated shell-based LUKS open loop. The CLI is now
the single source of truth for unlock logic.

**2. USB mount unit** (conditional on `cfg.autoUnlock.enable`):
```nix
fileSystems."/run/braid-key" = lib.mkIf cfg.autoUnlock.enable {
  device = cfg.autoUnlock.keyDevice;
  fsType = "auto";
  options = [
    "ro" "nosuid" "nodev" "noexec"
    "nofail"    # never fail boot
    "noauto"    # only mount on-demand
    "x-systemd.device-timeout=${toString cfg.autoUnlock.timeoutSec}s"
  ];
};
```
No `uid/gid/umask` options — those are vfat/ntfs-specific and cause ext4/xfs
mounts to fail with invalid option errors (breaking auto-unlock silently).
Access control is handled by the mount point permissions (`/run/braid-key`
is `0700 root:root` via tmpfiles rule below), which gates access regardless
of the USB filesystem type.

**3. tmpfiles rule** for mount point permissions:
```nix
systemd.tmpfiles.rules = lib.mkIf cfg.autoUnlock.enable [
  # 0700 root:root — keyfile is sensitive; non-root must not traverse.
  # See docs/luks-unlock.md § "Mount point permissions".
  "d /run/braid-key 0700 root root -"
];
```

**4. braid-auto-unlock service**:
```nix
systemd.services.braid-auto-unlock = lib.mkIf cfg.autoUnlock.enable {
  description = "Auto-unlock braid pool from USB keyfile";
  wantedBy = [ "multi-user.target" ];
  after = [ "local-fs.target" ];
  unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
  # No RemainAfterExit — if USB is absent at boot (service exits 0 on
  # skip), a later `systemctl start braid-auto-unlock` must be able to
  # re-run the service when the USB is inserted. With RemainAfterExit=true,
  # systemd considers the unit "active" after exit 0 and suppresses
  # subsequent starts. See systemd.service(5).
  serviceConfig = { Type = "oneshot"; };
  path = [ cfg.package cfg.packages.cryptsetup cfg.packages.btrfsProgs cfg.packages.utilLinux ];
  script = let keyPath = "/run/braid-key/${cfg.autoUnlock.keyFile}"; in ''
    # Mount USB via systemd mount unit — this respects the device-timeout
    # configured on the mount unit, so slow USB enumeration gets the full
    # wait window. A direct `mount` call would bypass that timeout.
    # The escaped unit name matches systemd's path encoding for
    # /run/braid-key → run-braid\x2dkey.mount.
    if ! ${pkgs.systemd}/bin/systemctl start run-braid\\x2dkey.mount 2>/dev/null; then
      echo "braid-auto-unlock: USB key device not available, skipping" >&2
      exit 0
    fi

    if [ ! -f "${keyPath}" ]; then
      echo "braid-auto-unlock: keyfile not found at ${keyPath}, skipping" >&2
      umount /run/braid-key 2>/dev/null || true
      exit 0
    fi

    # Reject symlinks — a USB-crafted symlink (e.g. braid.key -> /etc/shadow)
    # would pass the Nix config assertion (no leading /, no ..) but resolve
    # outside /run/braid-key at runtime. CWE-59 (symlink following).
    if [ -L "${keyPath}" ]; then
      echo "braid-auto-unlock: keyfile is a symlink, refusing (CWE-59)" >&2
      umount /run/braid-key 2>/dev/null || true
      exit 0
    fi

    # Warn if keyfile is world/group-readable. On vfat (no Unix perms),
    # files are typically 0755 — we can't fix that (vfat doesn't support
    # chmod), so warn rather than fail. The mount point perms (0700) and
    # short mount window limit exposure. Hard-failing here would break
    # the most common USB format.
    perms=$(${pkgs.coreutils}/bin/stat -c '%a' "${keyPath}" 2>/dev/null || echo "???")
    case "$perms" in
      400|600) ;; # good
      *) echo "braid-auto-unlock: WARNING: keyfile perms are $perms (expected 400)" >&2 ;;
    esac

    if braid unlock --key-file "${keyPath}"; then
      echo "braid-auto-unlock: pool unlocked successfully" >&2
    else
      echo "braid-auto-unlock: unlock failed (wrong keyfile?), skipping" >&2
    fi

    # Always unmount USB after use. Never leave keyfile accessible — this
    # is the Unraid CVE pattern (plaintext credential on a mounted FS).
    # See docs/luks-unlock.md § "Plaintext keyfile exposure".
    umount /run/braid-key 2>/dev/null || true
    exit 0
  '';
};
```

**5. braid-pool.target** (line 144-148) — unchanged. Note: target stays
inactive after auto-unlock because nothing starts it. This is intentional —
the target is for manual workflows. Operators check mount state with
`mountpoint -q ${cfg.mountPoint}`.

## Phase 6: Tests (TDD)

Write failing tests first per Principle 8. Register in `flake.nix` following
the existing `pkgs.testers.nixosTest(import ... { braid = linuxCrane.braid; })`
pattern (flake.nix:88-128).

Every test file must start with the repo-required block comment (AGENTS.md):
intent, why it exists, and scenario. The descriptions below are the content
for those comments.

### CLI tests

**tests/cli/braid-enroll-key-file.nix + .py**
```python
# Test: braid-enroll-key-file
#
# Intent: Verify that `braid enroll-key-file` enrolls a binary keyfile into
# LUKS slot 1 on all pool disks, and that `braid unlock --key-file` can
# subsequently open them.
#
# Why it exists: The keyfile enrollment path uses different cryptsetup
# semantics than passphrase (raw bytes, explicit slot, no PBKDF). If
# enrollment silently fails or targets the wrong slot, auto-unlock breaks
# at 3 AM when nobody is watching.
#
# Scenario: 2-disk RAID1 pool. Generate 4096-byte random keyfile. Enroll
# into both disks. Lock pool. Unlock with keyfile. Verify data intact.
# Re-enroll (idempotent). Verify passphrase path still works.
```
- Setup: 2-disk RAID1 pool, generate 4096-byte random keyfile.
- Subtests: enroll → lock → unlock with keyfile → data intact → re-enroll
  is idempotent → passphrase still works.

**tests/cli/braid-unlock-key-file.nix + .py**
```python
# Test: braid-unlock-key-file
#
# Intent: Verify `--key-file` flag opens LUKS with a binary keyfile and
# that a wrong keyfile is rejected.
#
# Why it exists: The keyfile unlock code path is entirely different from
# passphrase (no PBKDF, different cryptsetup flags, run() vs run_with_stdin).
# Must verify independently that correct keyfile succeeds, wrong keyfile
# fails, and passphrase enrollment is not corrupted by keyfile enrollment.
#
# Scenario: Pool set up with passphrase (slot 0) and keyfile (slot 1).
# Lock. Unlock with correct keyfile. Lock. Try wrong keyfile (fail).
# Unlock with passphrase (still works).
```
- Setup: pool with both passphrase (slot 0) and keyfile (slot 1).
- Subtests: correct keyfile → success; wrong keyfile → failure; passphrase
  path unaffected.

### Module tests

**tests/module/auto-unlock-key-present.nix + .py**
```python
# Test: auto-unlock-key-present
#
# Intent: Verify that when autoUnlock is enabled and a USB device with a
# valid keyfile is present at boot, the pool is automatically mounted and
# the USB is unmounted after use.
#
# Why it exists: This is the primary auto-unlock use case. If systemd
# service ordering, mount unit config, or keyfile path resolution is wrong,
# users get a locked NAS after an unattended reboot.
#
# Scenario: NixOS module test. Virtual "USB" disk (serial "usbkey")
# formatted ext4 containing /braid.key. Fixture pre-creates LUKS+btrfs
# pool and enrolls keyfile. VM boots to multi-user with pool mounted.
# USB is NOT still mounted at /run/braid-key.
```
- Setup: extra virtual disk (serial "usbkey") formatted ext4 with
  `/braid.key`. Fixture pre-creates pool and enrolls keyfile.
  `braid.autoUnlock.enable = true; keyDevice = "/dev/disk/by-id/virtio-usbkey";`
- Assert: pool mounted after boot. USB NOT still mounted at `/run/braid-key`.

**tests/module/auto-unlock-key-missing.nix + .py**
```python
# Test: auto-unlock-key-missing
#
# Intent: Verify that when autoUnlock is enabled but no USB device is
# present, boot succeeds normally with the pool locked.
#
# Why it exists: Principle 1 (resilient by default). A missing USB key
# must NEVER block boot or cause systemd to enter degraded state.
#
# Scenario: Same module config as key-present test, but no usbkey virtual
# disk attached. VM boots to multi-user. Pool is NOT mounted. System is
# SSH-accessible and functional.
```
- Setup: same module config, no usbkey disk attached.
- Assert: boot completes. Pool not mounted. System functional.

**tests/module/auto-unlock-key-wrong.nix + .py**
```python
# Test: auto-unlock-key-wrong
#
# Intent: Verify that when autoUnlock is enabled and a USB device is
# present but contains a wrong/invalid keyfile, boot succeeds with the
# pool locked.
#
# Why it exists: A corrupted or swapped USB must not block boot, cause
# error loops, or leave the system in a degraded state.
#
# Scenario: USB disk is present but keyfile has wrong content (random
# bytes, not enrolled in pool). Boot completes. Pool not mounted.
# Warning in journal.
```
- Setup: usbkey disk has random bytes not enrolled in pool.
- Assert: boot completes. Pool not mounted. Warning in journal.

### Regression

Run full existing suite (`just test`) to confirm no regressions from the
braid-unlock service refactor.

## Phase 7: Documentation

**docs/principles.md** (line 26) — replace "Keyfile support will be adopted
after v1.0 release." with reference to `braid enroll-key-file` and
`braid.autoUnlock`.

**README.md** — add "Auto-unlock with USB keyfile" section:
- Key generation: `dd if=/dev/urandom of=/mnt/usb/braid.key bs=4096 count=1 iflag=fullblock`
  Use `/dev/urandom`, not `/dev/random` — on modern kernels (5.6+) they use
  the same CSPRNG, but `/dev/urandom` never blocks on entropy-starved systems
  (VMs, embedded, early boot). `iflag=fullblock` guarantees a complete 4096-byte
  read. This matches cryptsetup man page and Arch Wiki recommendations.
- `chmod 400 /mnt/usb/braid.key`
- Enrollment: `sudo braid enroll-key-file /mnt/usb/braid.key`
- NixOS config snippet
- Best-effort semantics (missing USB = boot continues, pool locked)
- Note: `braid-pool.target` does not reflect auto-unlock state
- Threat model note: "For maximum security, remove the USB key after the
  pool unlocks. If the USB remains in the server, an attacker who steals
  both the server and USB can unlock all drives — the encryption provides
  no protection against physical theft of the combined unit."

**docs/1-user-stories.md** — add boot-with-USB-key flow (present / absent).

## Dependency graph

```
Phase 1 (cmd.rs, luks.rs)
  ├─→ Phase 2 (unlock --key-file)
  │     └─→ Phase 5 (NixOS module)
  │           └─→ Phase 6: module tests
  └─→ Phase 3 (enroll-key-file cmd)
        ├─→ Phase 4 (add --enroll-key-file)
        └─→ Phase 6: CLI tests

Phase 7 (docs) — parallel with any phase
```

## Verification

1. `cargo test` — unit tests for new CmdRequest variants and luks helpers.
2. `just test braid-enroll-key-file braid-unlock-key-file` — CLI keyfile
   tests pass.
3. `just test auto-unlock-key-present auto-unlock-key-missing auto-unlock-key-wrong`
   — module tests pass.
4. `just test` — full suite, no regressions from braid-unlock refactor.
