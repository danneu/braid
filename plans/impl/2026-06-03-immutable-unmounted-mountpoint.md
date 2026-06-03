# Immutable mountpoint while unmounted

Status: proposal (implementation plan). Date: 2026-06-02. Single-pool model.

## Context

A pool mountpoint (default `/mnt/storage`) is a plain directory on the root
filesystem. When the pool is mounted there, writes go to the pool; when it is
NOT mounted, that bare directory is still writable, so any process writing under
the path silently lands data on the ROOT disk. When the pool later mounts over
it, that data is shadowed (invisible), permanently consumes root space, and the
write looked like it succeeded. This is the classic "unmounted mountpoint"
data-safety bug.

Fix: set the inode immutable attribute (`FS_IMMUTABLE_FL`, a.k.a. `chattr +i`)
on the bare mountpoint directory while it is unmounted.

- Unmounted: create/write under the directory fails immediately with `EPERM`.
- A filesystem can still be mounted OVER an immutable directory; once mounted,
  the mounted filesystem's own root inode governs writes, so normal pool writes
  work.
- The attribute is persistent inode metadata (survives unmount and reboot).
- Setting it requires `CAP_LINUX_IMMUTABLE`; braid already runs privileged.

braid is the correct owner because the invariant has a hard timing rule:

> Only ever set `+i` when the path is NOT currently a mountpoint. Setting it on a
> mounted path seals the MOUNTED filesystem's own root inode -- blocking all pool
> writes and persisting on the pool until cleared.

braid knows the mount state and controls the lifecycle, so it can honor that rule
reliably: the boot/activation unit seals the bare mountpoint while it is offline,
re-running on every boot AND every `nixos-rebuild switch` (self-healing). A bare
tmpfiles `chattr +i` hack cannot honor the timing rule -- it would seal the live
pool root during a `nixos-rebuild switch` performed while the pool is mounted --
whereas braid's unit gates on `ConditionPathIsMountPoint=!` plus the fd
`STATX_ATTR_MOUNT_ROOT` check, so it only ever seals the offline bare dir.

### Mechanism verification (already confirmed against the pinned kernel)

- Mount-over-immutable is allowed: there is no `IS_IMMUTABLE` check in the mount
  path (`reference/linux/fs/namespace.c` has none; the guard lives only in
  `fs/attr.c`).
- `+i` blocks metadata writes: `may_setattr` (`reference/linux/fs/attr.c:367-369`)
  returns `-EPERM` for `chmod`/`chown`/explicit-time changes on an immutable
  inode. This is why the tmpfiles interaction below matters.

### Decisions taken (confirmed with the user)

1. **Non-configurable (always-on).** The seal is an unconditional safety
   invariant, in the same class as the baked-in base mount options set
   unconditionally in `base_mount_options()` -- `noatime` (rationale in ADR-015,
   HDD defaults) and `skip_balance` (Principle in `docs/design/principles.md`) --
   there is no `immutableWhenUnmounted` knob. This **supersedes** the earlier
   reversible-opt-out decision. Rationale: there is no legitimate "off" use case
   (writing the bare offline mountpoint *is* the bug); the failure escape-hatches
   that matter -- graceful degradation on unsupported fs / old kernel
   (`Unsupported`/`MountStateUnknown`) and the `seal-mountpoint --unseal <path>`
   lever -- exist independently of any flag; and braid's no-backwards-compat
   stance means the knob can be added back trivially if a concrete need ever
   surfaces.
2. **Close the boot window.** Add a boot-time seal so the invariant holds from
   boot, not only after the first unlock. A NAS waiting for SSH unlock (auto-
   unlock off or USB key absent -- `braid-auto-unlock.service` exits 0 on skip,
   `modules/braid/storage.nix:200-247`) otherwise sits offline-and-writable
   indefinitely, and pre-mount sealing never fires because nothing mounts.
3. **Seal from the boot/activation unit ONLY -- not at create, not in the mount
   path.** The seal lives in exactly one place: the `braid-seal-mountpoint`
   oneshot. `braid add` does NOT seal, and neither does `scan_and_mount`. This is
   not a coverage gap -- a create-time seal would be a redundant `AlreadyImmutable`
   no-op -- for two compounding reasons:
   - **The oneshot runs on every activation, not just reboot.**
     `braid-seal-mountpoint.service` is `Type=oneshot` with no `RemainAfterExit`,
     so NixOS starts it on every `nixos-rebuild switch`/`test` as well as every
     boot: without `RemainAfterExit` a oneshot returns to `inactive (dead)` once
     `ExecStart` exits (`reference/systemd/man/systemd.service.xml`), and a switch
     (re)starts any unit `wantedBy` an active target that is not currently active,
     so the dead oneshot runs again on every activation. You cannot enable braid or
     change `braid.mountPoint` without an activation that runs the seal. (VM case 10
     pins this behaviorally -- out-of-band `chattr -i` while offline, then
     `switch-to-configuration test`, then assert re-sealed -- so the activation re-run
     is tested, not only asserted.)
   - **The mountpoint is static and pre-exists every pool.** `cfg.mountPoint` is a
     single fixed path created by the tmpfiles rule `d ${cfg.mountPoint}` on every
     boot/activation, so the seal unit seals it (while offline, gated by
     `ConditionPathIsMountPoint=!` plus the fd `STATX_ATTR_MOUNT_ROOT` check)
     BEFORE any `braid add` can run. The pool then mounts OVER the already-sealed
     dir; `+i` persists on the underlying inode (kernel-verified), and braid's
     lock/unmount path never `rmdir`s or `chmod`/`chown`s the bare dir
     (`online_state.rs#mark_online` fixups are gated to the mounted pool root), so
     the next `braid lock` reveals it still sealed.

   So any pool bootstrapped after braid is enabled inherits an already-sealed
   mountpoint. The only residual window -- an activation's seal genuinely failed,
   before the next activation, with a writer active -- is the same rare /
   self-healing / doctor-detected class this decision already tolerates for an
   out-of-band unseal, and a create seal using the same `enforce` logic would not
   help against the real failure modes (unsupported fs / old kernel) anyway. The
   boot unit is ordered `before braid-auto-unlock.service` so it runs in the
   pre-mount window even on auto-unlock-with-USB systems that otherwise never boot
   offline (see Insertion points). The doctor "offline + mutable -> Warn" check is
   the detection/self-heal signal for the rare out-of-band unseal; the next
   activation re-seals. (Kernel basis: `+i` survives unmount and reboot, and the
   kernel refuses `rmdir` of an immutable dir -- `may_delete` -> `IS_IMMUTABLE` ->
   `-EPERM` -- so a sealed offline mountpoint cannot be silently removed and
   recreated mutable while offline.)

## Design overview

One guarded primitive, called from the single point that establishes the
persistent seal -- the boot/activation systemd unit (`braid seal-mountpoint`):

```
seal_offline_mountpoint(path, guard):
    outcome = guard.enforce(path, want_immutable = true)   # always set +i
    log/warn based on outcome; never fail the caller
```

`guard.enforce` performs the timing rule atomically with an fd-based mountpoint
check (no shell-out, no TOCTOU): open the dir, confirm it is NOT a mount root on
that same fd via `statx`'s `STATX_ATTR_MOUNT_ROOT`, then GET/SET flags on that fd.
If it IS a mount root, do nothing. `STATX_ATTR_MOUNT_ROOT` is authoritative --
unlike a bare `st_dev`-vs-parent comparison it also detects same-device and bind
mountpoints (util-linux falls back to `st_dev` only without `/proc`, and its own
comment notes that fallback "is ... not able to detect bind mounts" --
`reference/util-linux/sys-utils/mountpoint.c:132,150`).

Call sites:

| When | Site | Purpose |
| --- | --- | --- |
| Boot / activation | new `braid seal-mountpoint` + oneshot unit | enforce invariant on every boot AND every `nixos-rebuild switch` |

This is the SOLE seal site. The create/bootstrap path (`pool_bootstrap_mount` /
`_raid1`) and the bring-online mount path (`scan_and_mount`, reached by unlock and
recover) are deliberately NOT seal sites -- see Decision 3. The static mountpoint
is sealed by the boot/activation unit before any `braid add` runs, and persistence
carries that seal across every later unlock/lock with no re-seal. Even the optional
post-unmount `mark_offline` re-assert is left OUT: re-sealing on every boot and
every activation already gives strong self-healing.

The attribute is persistent, so once the boot/activation unit seals the dir, it
stays sealed across later unmounts and reboots automatically. NO Rust code outside
the new module is touched for sealing: `scan_and_mount`, `execute_mount_only` /
`execute_unlock_and_mount`, `unlock.rs`, `recover.rs`, `test_fixtures/mount.rs`,
`pool.rs`, and `add.rs` carry NO `guard` parameter and NO seal call. The only
constructors of a `RealMountpointGuard` are the boot unit's `seal-mountpoint`
subcommand and the doctor check, each constructing it directly. The safety
mechanism is the persistent `+i` set by the boot/activation unit -- there is no
mount-path, create-path, or preview integration at all.

## KEY AUDIT result -- nothing writes into the mountpoint before mount

Confirmed: no code writes a file INTO the mountpoint directory before the pool is
mounted over it, so `+i` breaks no existing pre-mount write.

- Pool lock: `/run/braid-pool.lock`; stop coordinator: `/run/braid-stop-coordinator.lock` (`cli/src/pool_lock.rs`).
- State (`pool.json`, `pending-op.json`, acked-stats, LUKS header backups): under `/var/lib/braid` (`cli/src/state_paths.rs`).
- The only pre-mount operation on the path is `std::fs::create_dir_all(mount_point)` (`cli/src/mount.rs:814`, `cli/src/pool.rs:614,648`), immediately followed by the mount.
- Post-mount `chmod(0o2770)`/`chown(root:group)` in `mark_online` (`cli/src/online_state.rs:266-290`) is gated behind `is_mountpoint == true` and targets the MOUNTED pool root inode, not the bare dir -- unaffected by `+i` on the (shadowed) bare dir.

### External writers (intended behavior change)

This audit covers braid's OWN pre-mount writes -- there are none in the
mountpoint, so `+i` breaks nothing braid does. It does NOT cover operator-
configured services. On a NAS, services like Samba/NFS exports, Syncthing,
Nextcloud, or cron/rsync backups are routinely `wantedBy multi-user.target` and
will start and write to `/mnt/storage` while the pool is offline (auto-unlock
skipped or USB key absent, awaiting SSH unlock). With `+i` those writes now fail
with `EPERM`. That is the intended win, not a regression: a loud `EPERM` replaces
the silent write-to-root that leaked space and got shadowed on mount -- exactly
the data-safety bug this plan fixes. It IS a behavior change for external writers
(not a no-op), so the ADR and README must state plainly that any process writing
the mountpoint while the pool is offline now fails with `EPERM` by design, and
that this is the desired outcome.

This covers ONLY the pool mountpoint `cfg.mountPoint` (`/mnt/storage`). Subvolume
mounts at SEPARATE root-fs paths -- the documented `systemd.mounts` +
`bindsTo = braid-online.service` pattern -- are NOT covered by the boot seal and
retain the offline-write-to-root exposure; see the Subvolume-mount caveat below.

### tmpfiles interaction (safe by construction)

The module keeps `d ${cfg.mountPoint} 0755 root root` (`modules/braid/storage.nix:46`).
systemd-tmpfiles only issues `chmod`/`chown` when the mode/owner DIFFER from the
target (`reference/systemd/src/tmpfiles/tmpfiles.c:966,973`: `do_chown`/`do_chmod`
are computed from a mismatch). tmpfiles creates the bare dir as `0755 root root`
and braid never changes the bare dir's mode/owner (it only fixes the mounted pool
root), so there is never a mismatch -> tmpfiles attempts neither call -> no
`EPERM` against `+i`. Caveat documented below: if an admin changes the tmpfiles
mode so it no longer matches the on-disk bare dir, tmpfiles would attempt a
`chmod`/`chown` and hit `EPERM` while offline.

## New module: `cli/src/mountpoint_guard.rs`

Mirrors the existing ioctl seam in `cli/src/btrfs_ioctl.rs` (trait + Real impl +
mock + `#[ignore]` root smoke test). No new `nix` features (reuses
`nix::ioctl_*!`, `nix::fcntl::{open, OFlag}` already used by `btrfs_ioctl.rs`).
`FS_IMMUTABLE_FL = 0x10` (`reference/linux/include/uapi/linux/fs.h:363`);
`FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS` are `_IOR('f',1,long)` / `_IOW('f',2,long)`
(`reference/linux/include/uapi/linux/fs.h:316-317`).

**Target-gating: the real syscall code and the ABI assertion are Linux-only.**
`just test-rust` runs host `cargo test` (`justfile#test-rust`) and the flake builds
for `aarch64-darwin` as well as `x86_64-linux` (`flake.nix` `forAllSystems`).
`btrfs_ioctl.rs` compiles on macOS only because it touches nothing Linux-specific --
cross-platform `nix` ioctl macros plus a platform-independent `size_of` assertion.
This module is different: `enforce`'s mount-root check uses `libc::statx` +
`STATX_ATTR_MOUNT_ROOT` + `AT_EMPTY_PATH`, all Linux-only libc symbols absent on
Darwin, and the ABI assertion below hardcodes the Linux LP64 request numbers
(`nix::request_code_*!` emits the BSD `_IOC` encoding on Darwin). So gate the
`statx`/`FS_IOC_*` `RealMountpointGuard` impl and the ABI assertion behind
`#[cfg(target_os = "linux")]`, and provide a non-Linux `RealMountpointGuard` stub
whose `enforce` returns `Ok(GuardOutcome::Unsupported)` and whose `is_immutable`
returns `Err(GuardError::Unsupported)` -- NOT `Ok(Unsupported)` for both, which would
not even compile: `is_immutable -> Result<bool, GuardError>` has no bool that honestly
means "unsupported", so it travels as `Err`, which the doctor already maps to
`Indeterminate` -> no finding (the same degradation as a real-Linux `ENOTTY`).
`GuardError` carries an `Unsupported` variant -- the one the real Linux `is_immutable`
also returns for `ENOTTY`/`EOPNOTSUPP` on a root fs without the attribute. The trait, `GuardOutcome`,
`seal_offline_mountpoint`, `MockMountpointGuard`, the pure doctor classifier, and the
`main.rs` dispatch tests are all platform-independent and stay cross-platform, so
`just test-rust` builds and runs the abstract half on the Darwin host while the real
ioctl round-trip is proven in the Linux VM tests.

**ABI width is load-bearing: the flags buffer and the nix ioctl-macro type MUST be
`libc::c_long`, not `c_int`.** The module defines ONE buffer-type alias --
`type FsFlagsArg = libc::c_long` -- and uses it for both
`ioctl_read!(fs_ioc_getflags, b'f', 1, FsFlagsArg)` and
`ioctl_write_ptr!(fs_ioc_setflags, b'f', 2, FsFlagsArg)`, so the request number is
derived from a single definition a regression can flip. The nix
`ioctl_read!`/`ioctl_write_ptr!` macros compute the request number from that buffer
type. A `c_int` (4 bytes) buffer encodes a request number that does not match the
kernel's `('f', N, long)` constant (8 bytes on LP64), so the `case FS_IOC_GETFLAGS:`
switch never matches -> `ENOTTY` -> `enforce` returns `Unsupported` -> protection is
silently inert. The kernel handler transfers only the low 32-bit `int`:
`ioctl_getflags` does `put_user(fa.flags, argp)` with `unsigned int __user *argp`,
and `ioctl_setflags` does `get_user(flags, argp)` into an `unsigned int`
(`reference/linux/fs/file_attr.c:309-329`). So a zeroed `c_long`, written/read on
little-endian aarch64/x86_64 (braid's only targets), carries the flags correctly in
its low bytes. Two complementary guards pin this: (1) a const-assertion unit test
(`just test-rust`, no root/VM) asserts `nix::request_code_read!(b'f', 1,
size_of::<FsFlagsArg>())` equals the kernel's `0x8008_6601` and
`nix::request_code_write!(b'f', 2, size_of::<FsFlagsArg>())` equals `0x4008_6602`
-- because it reads `size_of::<FsFlagsArg>()`, a regression flipping the alias to
`c_int` yields `0x8004_6601`/`0x4004_6602` and fails immediately, the fast feedback
the mock interaction tests cannot give (they bypass the real request number); (2) VM
case 1 exercises the real kernel ioctl round-trip, proving the number is not merely
arithmetically correct but actually accepted by the running kernel. Gate guard (1)
behind `#[cfg(target_os = "linux")]` -- the hardcoded `0x8008_6601`/`0x4008_6602`
are the Linux LP64 numbers, and on the `aarch64-darwin` `just test-rust` host
`nix::request_code_*!` emits the BSD encoding; whether written as a
`const _: () = assert!(...)` or a `#[test]`, it must be cfg-gated so the BSD-encoded
macro never reaches the Darwin compiler (F1 target strategy). It still runs in the
`x86_64-linux` `just test-rust` lane, which is where a `c_long`->`c_int` regression
would land. The mock unit tests alone still cannot catch an ABI regression -- they bypass the ioctl entirely
(see test matrix).

```rust
pub enum GuardOutcome { Set, Cleared, AlreadyImmutable, AlreadyMutable,
                        SkippedMounted, Absent, Unsupported, MountStateUnknown,
                        NotADirectory }

pub trait MountpointGuard {
    /// Enforce immutability on `path`, but ONLY when `path` is not currently a
    /// mountpoint. The not-a-mountpoint check and the flag write happen on the
    /// same fd so a racing mount can never cause us to seal a live fs root.
    fn enforce(&self, path: &Path, want_immutable: bool) -> Result<GuardOutcome, GuardError>;
    /// Read current immutability (for doctor). The doctor maps `Err` (absent /
    /// unsupported / old kernel / I/O) to an `Indeterminate` probe, not a failure.
    fn is_immutable(&self, path: &Path) -> Result<bool, GuardError>;
}

pub struct RealMountpointGuard;          // Linux: fd + ioctl. non-Linux stub: enforce -> Ok(Unsupported), is_immutable -> Err(Unsupported)
// MockMountpointGuard (test): returns configured outcomes, records calls.

/// Best-effort enforcement wrapper for the BARE (boot/configured-path) form of
/// `seal-mountpoint` (Decision 3 -- the sole seal site; not create, not the mount
/// path). Never fails the caller and always exits 0 -- a missing guard must not block
/// boot. Always requests `want_immutable = true`; the seal is non-configurable. The
/// explicit-path forms (`seal-mountpoint <path>` and `--unseal <path>`) do NOT route
/// through this wrapper: they call `enforce` directly and map the `GuardOutcome` to an
/// honest desired-state exit code (F2), since an operator remediation must surface a
/// failed seal/clear rather than swallow it into a best-effort log.
pub fn seal_offline_mountpoint(path: &Path, guard: &dyn MountpointGuard);
```

`RealMountpointGuard::enforce`:
1. `open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)`; `ENOENT` -> `Ok(Absent)`;
   `ENOTDIR` -> `Ok(NotADirectory)` (path exists but is not a directory -- a typo or
   misconfigured `braid.mountPoint`; refuse to seal a regular file, since the
   invariant is about mountpoint DIRECTORIES). `O_DIRECTORY` makes the kernel enforce
   directory-ness at `open`; `O_CLOEXEC` keeps the fd from leaking across any later
   exec.
2. authoritative fd-based mount-root check: `statx(fd, "", AT_EMPTY_PATH, ...)`
   (via `libc::statx`; the pinned kernel reports `STATX_ATTR_MOUNT_ROOT = 0x2000`
   in `stx_attributes` -- `reference/linux/include/uapi/linux/stat.h`). If
   `stx_attributes & STATX_ATTR_MOUNT_ROOT` (path is a mount root -- when mounted,
   `open` follows into the pool root) -> `Ok(SkippedMounted)`, never touch flags.
   This catches bind/same-device mountpoints that an `st_dev`-only check would
   miss. Gate on `stx_attributes_mask & STATX_ATTR_MOUNT_ROOT`; if the kernel does
   not report the bit (e.g. a kernel older than the pinned one), fail closed to
   `Ok(MountStateUnknown)` -- make no flag change, but surface a WARNING rather
   than the silent debug `SkippedMounted`, so an unexpected kernel that leaves
   protection inert is loud, not invisible (mirrors systemd treating a missing
   mandatory statx attribute as `-EUNATCH` / "old kernel?",
   `reference/systemd/src/basic/stat-util.c:386-393`). If a mount races in AFTER open,
   the fd still refers to the underlying bare-dir inode (not a mount root), so
   step 4 seals the bare dir (correct), never the pool root.
3. `FS_IOC_GETFLAGS(fd)`; `ENOTTY`/`EOPNOTSUPP` -> `Ok(Unsupported)` (root fs does
   not support the attribute -- e.g. some tmpfs/zfs roots).
4. compute desired flags (`| FS_IMMUTABLE_FL` if `want_immutable`, else `& !`); if
   unchanged -> `Ok(AlreadyImmutable|AlreadyMutable)`, else `FS_IOC_SETFLAGS(fd)`
   -> `Ok(Set|Cleared)`. `EPERM` -> `GuardError`.

`seal_offline_mountpoint` maps outcomes to logging: `Set`/`Cleared` -> info;
`SkippedMounted`/`Already*`/`Absent` -> debug; `NotADirectory` -> one clear WARNING
("mountpoint path is not a directory -- refusing to set immutable; check
braid.mountPoint"); `Unsupported` -> one clear WARNING
("root filesystem does not support the immutable attribute -- unmounted-mountpoint
protection unavailable"); `MountStateUnknown` -> one clear WARNING ("cannot
determine mountpoint mount state (kernel lacks STATX_ATTR_MOUNT_ROOT) --
unmounted-mountpoint protection unavailable; no attribute change made"); `Err` ->
WARNING + continue.

## Insertion points

- Boot unit + subcommand (the SOLE seal site):
  - New **visible** (NOT `hide`-d) subcommand `braid seal-mountpoint`
    (`cli/src/main.rs`, clap arm + dispatch like the existing `lock --systemd-stop`
    internal path, but listed in `braid --help`): **lock-free** -- load config, call
    `seal_offline_mountpoint(mount_point, &RealMountpointGuard)`, always exit 0 (boot
    must not fail on a best-effort guard). It must be visible because three
    operator-facing surfaces direct operators at it -- the offline+`Mutable` doctor
    Warn hint ("run `braid seal-mountpoint`"), the mounting-subvolumes remedy
    (`seal-mountpoint <path>`), and the reconfiguration caveat
    (`seal-mountpoint --unseal <path>`) -- so a `hide`-d command those hints name but
    `--help` omits would be an undiscoverable dead end. Unlike `lock --systemd-stop`
    (a pure internal flag), `seal-mountpoint` has operator-facing maintenance forms.
    Visibility adds no footgun: the bare form is offline-gated (fd mount-root check +
    `ConditionPathIsMountPoint`) and idempotent (`AlreadyImmutable`/`SkippedMounted`
    no-op). Do NOT acquire the
    pool lock: `braid-seal-mountpoint` and `braid-auto-unlock` are both pulled in
    by `multi-user.target`, so a lock-holding seal could make a racing
    `braid unlock --key-file` fast-fail on contention and leave an unattended NAS
    locked. The fd mount-root guard already makes the seal safe under any race
    (it never touches a mounted inode), and `ConditionPathIsMountPoint` is the
    coarse second gate -- the pool lock adds risk here without adding safety.
    Classify the bare and explicit-path SEAL branches as `LockPolicy::None`
    (`cli/src/main.rs#lock_policy`); the `--unseal` branch is `LockPolicy::NonBlocking`
    (see Maintenance mode below -- it is a lock-acquiring remediation, not a
    best-effort boot action). Pin every `seal-mountpoint` branch in
    `lock_policy_classifies_every_command_and_branch`
    (`cli/src/main.rs#lock_policy_classifies_every_command_and_branch`).
  - Maintenance mode (explicit-path lever, symmetric seal/unseal): the operator's
    braid-native broom, since the appliance wrapper PATH has no `chattr`.
    - `braid seal-mountpoint <path>` takes an EXPLICIT path (not the configured
      `mount_point`) and SETS `+i`, reusing the same fd-guarded
      `enforce(path, true)`. This is the remedy the mounting-subvolumes guide
      points operators at to protect subvolume mountpoints at separate root-fs
      paths (Subvolume-mount caveat below), which the boot oneshot does not cover.
      Because it is an operator remediation (not the best-effort boot seal) for paths
      the doctor cannot see, it reports HONEST desired-state exit codes: exit 0 iff the
      path ends up immutable (`Set` OR `AlreadyImmutable`), and non-zero on
      `SkippedMounted` / `Absent` / `Unsupported` / `MountStateUnknown` /
      `NotADirectory` / `Err`, so a manual seal that silently failed to protect a
      subvolume mountpoint is visible rather than a green no-op (F2). It stays
      lock-free (`LockPolicy::None`): sealing is monotonic toward more-protected and
      the fd mount-root guard already refuses a live mount (`SkippedMounted`), so
      unlike `--unseal` there is no post-`open` race that could leave the path
      unprotected, hence no pool lock is needed.
    - `braid seal-mountpoint --unseal <path>` takes an EXPLICIT path and CLEARS
      `+i` (`enforce(path, false)`) -- the lever for the reconfiguration caveat
      below. Unlike the seal forms, `--unseal` is an operator REMEDIATION, not a
      best-effort boot action, so it does NOT inherit their lock-free/always-0
      contract. It:
      - (a) ACQUIRES the pool lock, fail-fast on contention -- classify the
        `--unseal` branch as `LockPolicy::NonBlocking` (`cli/src/main.rs#lock_policy`),
        so dispatch's `acquire_pool_or_exit` (`cli/src/main.rs#acquire_pool_or_exit`)
        takes the same `/run/braid-pool.lock` that `unlock` (also `NonBlocking`) and
        plain `lock` contend on. `lock` is `LockPolicy::LockPlain`, NOT `NonBlocking`,
        but its handler acquires that identical pool lock via `acquire_pool_or_exit`
        inside `run_plain_lock` (`cli/src/main.rs#run_plain_lock`), so `--unseal` still
        serializes against an in-flight unlock or lock. This forecloses the post-`open`
        race where a concurrent mount lands the pool OVER a just-cleared bare dir,
        leaving it mutable-and-shadowed after the next lock;
      - (b) REFUSES when `<path>` resolves to the current `cfg.mount_point()` -- the
        live configured path must stay sealed while offline (clearing it just
        reopens the bug until the next activation re-seals), so this is a hard
        error, never a clear;
      - (c) EXITS NON-ZERO unless the requested end-state (mutable) actually holds --
        `Cleared` OR `AlreadyMutable` (an already-mutable path means the unseal request
        is already satisfied, so a repeat `--unseal` on a cleared orphan reports
        success, not failure -- F2): `SkippedMounted` (path is a live mountpoint),
        `Absent`, `Unsupported`, `MountStateUnknown`, `NotADirectory`, and `Err` all
        surface as failures, so a remediation that did not achieve the requested
        mutable state never masquerades as success.
      This is NOT in tension with the bare-form no-lock rationale above: that
      rationale is specific to the boot oneshot racing `braid-auto-unlock` (a
      lock-holding boot seal could fast-fail an unattended `braid unlock --key-file`
      and strand the NAS locked); `--unseal` is never run by a boot unit, so
      acquiring the lock there adds the needed serialization without the boot
      footgun.
    Both explicit-path forms are guarded by the same `STATX_ATTR_MOUNT_ROOT` fd
    check, so they refuse a mounted path (`SkippedMounted`) and only ever touch an
    offline bare dir. Only the bare (no-arg) boot form is always-exit-0 best-effort
    (boot must not fail on the guard); BOTH explicit-path forms report honest
    desired-state exit codes (seal: 0 iff `Set | AlreadyImmutable`; unseal: 0 iff
    `Cleared | AlreadyMutable`; every skipped/absent/unsupported/unknown/
    not-directory/error outcome is non-zero -- F2). The bare form and the
    explicit-path SEAL form are `LockPolicy::None` (lock-free); the `--unseal` form is
    `LockPolicy::NonBlocking` per the split just described. The bare (no-arg) form
    still seals the configured `mount_point` and remains the boot/internal entry.
  - New oneshot `systemd.services."braid-seal-mountpoint"` in
    `modules/braid/storage.nix`, modeled on `braid-unlock.service`/`braid-auto-unlock.service`:
    `Type=oneshot`, `ConditionPathIsMountPoint = "!${cfg.mountPoint}"` (only when
    offline -- belt-and-suspenders alongside the in-CLI fd check, and prevents
    sealing during a mounted `nixos-rebuild switch`), `after = [ "local-fs.target"
    "systemd-tmpfiles-setup.service" ]`, `wantedBy = [ "multi-user.target" ]`, and
    -- load-bearing under Decision 3 -- `before = [ "braid-auto-unlock.service" ]`.
    Invoke the CLI via NixOS `path = [ braidWrapped ]` + `script = "braid
    seal-mountpoint"`, NOT a raw `ExecStart = braid seal-mountpoint`: like
    `braid-unlock`/`braid-auto-unlock` (`modules/braid/storage.nix#braid-unlock`),
    the unit must define its command through `script`, which compiles to an absolute
    generated-script `ExecStart` and resolves `braid` through the unit `PATH` that
    `path` populates. A bare relative `ExecStart = braid ...` would not resolve:
    systemd looks up a non-absolute command in a search path FIXED AT COMPILE TIME
    (`/usr/bin:/bin:...`), NOT the service `PATH` (`reference/systemd/man/systemd.service.xml:1492`),
    so the nix-store wrapper would not be found and the seal unit would fail before
    sealing anything. Only `braidWrapped` is needed in `path` -- the seal is pure
    syscalls (open/statx/ioctl), with no shell-out to cryptsetup/btrfs/util-linux.
    The `before` edge is the seal's correctness guarantee, not a nicety:
    `braid-auto-unlock.service` (`storage.nix:198-202`) has IDENTICAL boot wiring
    (`wantedBy multi-user.target`, `after local-fs.target`,
    `ConditionPathIsMountPoint=!${cfg.mountPoint}`) and mounts the pool by running
    `braid unlock --key-file`. Without an ordering edge the two oneshots race; if
    auto-unlock wins, it mounts the pool and the seal unit's
    `ConditionPathIsMountPoint=!` then fails, so the seal is SKIPPED. An
    auto-unlock-with-USB NAS comes online on every boot and thus never boots
    offline, so without this edge nothing ever seals the bare dir and a later
    `braid lock` reopens exactly the bug this plan fixes. Ordering the seal before
    auto-unlock makes it run in the pre-mount offline window every boot;
    auto-unlock then mounts OVER the sealed dir (mount-over-immutable is verified)
    and persistence carries it. "Ordered before pool consumers" does NOT cover
    this: consumers bind to `braid-online.service` (`storage.nix:130`,
    `ConditionPathIsMountPoint=${cfg.mountPoint}`), which is DOWNSTREAM of the
    mount -- the racer is the upstream mounter. When `autoUnlock` is disabled the
    unit does not exist and `before` is a harmless no-op ordering string. (The only
    other unlock entry, `braid-unlock.service` at `storage.nix:153`, is pulled by
    the manual `braid-pool.target`, not `multi-user.target`, so it is not an
    at-boot mounter and needs no ordering.)
- `mark_offline` re-assert: deliberately NOT done. A post-unmount re-seal would
  add guard threading into `online_state.rs` for no real gain -- the oneshot
  re-runs on every boot and every activation, and persistence holds `+i` across
  unmounts, so the only window it could close (out-of-band unseal, then unmount,
  before the next activation, with a writer active) is already the doctor-detected
  / next-activation-healed class Decision 3 tolerates. Leaving it out keeps the
  entire feature out of the mount/unlock/lock call graph.

## Dry-run / preview (decision 022)

Nothing to integrate. No braid plan-and-execute command seals the mountpoint, so
ADR 022 imposes no obligation here: the seal is an ambient systemd-unit-managed
invariant (the same class as the tmpfiles `d ${cfg.mountPoint}` rule), applied by
the boot/activation oneshot outside the plan/preview/execute model. `AddPlan`,
`compile_open_steps`, and the recover renderers are unchanged, and there is no
preview step in any command.

## Doctor checks (`cli/src/doctor.rs`)

Under Decision 3 (boot/activation is the sole seal site) this check is the ONLY
non-boot detection signal for a mountpoint left mutable out-of-band: it warns
while offline-and-mutable, and the next boot or activation re-seals. `DoctorContext`
stores a concrete `online_ops: RealOnlineStateOps<'a>`
(`cli/src/doctor.rs` struct def), which is not injectable, so the branch logic
must NOT live inline in the check. Extract a **pure decision helper** -- e.g.
`fn classify_mountpoint_immutability(mounted: Option<bool>, probe: ImmutabilityProbe) -> ImmutableFinding`
returning a typed `{ None | Warn(msg) | Failure(msg) }` -- mirroring the
pure-parser + thin-IO-shim split already used in `mount_check.rs`.

The immutability input is a 3-way `ImmutabilityProbe { Immutable | Mutable |
Indeterminate }`, NOT a bare `bool`. `guard.is_immutable(mount_point)` returns
`Result<bool, GuardError>`, and that probe can legitimately fail to yield a bool
-- mountpoint absent (`ENOENT`), root fs unsupported (`ENOTTY`), old kernel, or
I/O error -- the exact failure modes the `enforce` path models as first-class
`Absent`/`Unsupported`/`MountStateUnknown` outcomes. The thin wrapper converts
the probe result into the enum (`Ok(true) -> Immutable`, `Ok(false) -> Mutable`,
`Err(_) -> Indeterminate`) AND derives a tri-state mount value `Option<bool>`
(mounted / not-mounted / can't-tell), then feeds both to the helper; the helper is
unit-tested without root or wiring. Mount state is ALSO tri-state because the mount
probe can itself fail. NB: do NOT source it from `ensure_mountpoint_is_mounted` --
that helper is right for mounted-ONLY checks (it maps a probe error to `Some(false)`
via `.unwrap_or(false)`, `cli/src/doctor.rs#ensure_mountpoint_is_mounted`, so an
error conservatively SKIPS a mounted-only action like `df`), but for THIS check a
collapsed `Some(false)` would masquerade as "offline" and could fire the misleading
offline+`Mutable` Warn when the pool is actually mounted but the probe failed.
Instead map `online_ops.is_mountpoint(mount_point)` directly -- `Ok(b) -> Some(b)`,
`Err(_) -> None` -- so a mount-probe failure becomes `None` and suppresses BOTH
severities. (`ensure_mountpoint_is_mounted` returns `None` only when config is
absent, not on a probe error, so it does not by itself preserve the indeterminacy
this check needs.) Branch table:

- `Some(false)` (offline) + `Mutable` -> Warn (invariant not yet held; self-seals
  on next boot -- the boot unit seals before any auto-unlock mount -- or run
  `braid seal-mountpoint` for an immediate reseal. NOT on `braid unlock`: under
  Decision 3 unlock no longer seals, so the hint must never tell the operator to
  unlock to clear the warning).
- `Some(true)` (online) + pool ROOT `Immutable` -> Failure (catastrophic: a live pool
  root is sealed; this is the timing-rule violation and should never happen with
  this code -- a tripwire for bugs or external interference).
- mount state `None` (the mount probe could not determine mounted-ness) -> None.
  We cannot honestly say "offline + mutable, reseal" (it might be mounted) nor
  "online + immutable, catastrophe" (it might be offline), so neither severity is
  defensible; emit no finding. Same indeterminacy discipline as the `Indeterminate`
  immutability row below; this is the row a bare-`bool` `mounted` could not express,
  and is why the helper takes `Option<bool>`.
- `Indeterminate` immutability (any mount state) -> None. The probe could not determine
  immutability, so there is no honest actionable hint. On the realistic trigger
  -- an `Unsupported` root fs -- the seal unit ALREADY emits the single clear
  "root filesystem does not support the immutable attribute" warning, so a doctor
  "mountpoint not immutable; run braid seal-mountpoint" Warn would be both
  contradictory and un-actionable (resealing cannot make an unsupported fs support
  `+i`). Mapping to None keeps the seal unit as the sole source of the
  "protection unavailable" signal and forecloses the unguided
  `is_immutable(p).unwrap_or(false)` (false Warn) vs `unwrap_or(true)` (silent
  suppression) coin-flip the bare-bool signature invited. (Optional debug line.)
- otherwise -> None.
- (optional) offline + bare dir non-empty -> Warn (pre-existing shadowed leak).

## Error / logging behavior

Best-effort everywhere: warnings to stderr/journal, never block mount, unlock, or
boot. Unsupported root fs -> one clear warning naming the consequence. `EPERM`
(missing capability) should not occur as root; if it does, warn. All user-facing
strings use `--`, not em-dash (CLI output style rule).

## Sole-mounter / fstab caveat

This invariant assumes braid is the only thing mounting the path. The module
already replaced the `fileSystems` entry (`modules/braid/storage.nix:39`), so braid
is the sole mounter by design -- there is no fstab entry racing it. If an operator
adds their own fstab line or mount unit for the pool, external mount/unmount can
bypass the seal and the invariant can drift; the doctor checks above are the
detection mechanism. Document this assumption in the new decision doc.

## Subvolume-mount caveat (separate-path subvolume mounts not auto-sealed)

The boot seal covers ONLY `cfg.mountPoint` (`/mnt/storage`). braid documents and
tests a pattern (`docs/guides/mounting-subvolumes.md`,
`tests/module/subvol-mount-lifecycle.nix`) that mounts subvolumes at SEPARATE
root-fs paths -- e.g. `/var/lib/jellyfin/media`, `/home/dan/my-movies` -- via
`systemd.mounts` with `bindsTo = braid-online.service`. When the pool is offline
those mount units are stopped, leaving bare root-fs directories at those paths, so
a process writing them while offline lands data on root and gets shadowed on the
next mount -- the identical bug this plan fixes for `/mnt/storage`, NOT covered by
the boot oneshot (it seals one static path).

Scope decision (boot-only, document + manual lever -- NOT a new declarative
subsystem in this plan):

- **Subvolumes mounted UNDER the sealed `/mnt/storage`** are inherently protected
  by the parent seal (the bare dir is the sealed mountpoint), and are the safe
  default; the guide should state this.
- **Subvolumes mounted at separate paths** are an advanced, operator-opt-in
  pattern. This plan does NOT auto-seal them; instead:
  1. The `mounting-subvolumes.md` guide and the new ADR document the limitation
     plainly: the seal protects the pool mountpoint, and separate-path subvolume
     mounts retain the offline-write-to-root risk.
  2. The guide points operators at the braid-native remedy -- the new explicit-path
     `braid seal-mountpoint <path>` form (Insertion points) -- to seal those
     mountpoints manually, symmetric with the existing `--unseal <path>` broom. That
     form reports honest desired-state exit codes (non-zero on
     `SkippedMounted`/`Absent`/`Unsupported`/`MountStateUnknown`/`NotADirectory`/`Err`
     -- F2), which matters here precisely because the doctor probes only
     `cfg.mountPoint` and cannot see these separate paths: a silent best-effort exit 0
     would hide an unprotected subvolume mountpoint with no other detection signal.
  Note the documented consumer wiring already binds the service to its mount unit
  (`bindsTo` + `ConditionPathIsMountPoint`), so the documented writer itself does
  not write while offline; the residual exposure is undocumented writers to those
  paths, for which a documented limitation + manual lever is a proportionate first
  remedy.

The manual lever is honestly half-protective (not self-healing, and the doctor
check -- which probes only `cfg.mountPoint` -- cannot see these paths). The
fully-declarative escalation is captured as a revisit-if in the ADR (a
`braid.extraSealedMountPoints` list the boot/activation oneshot would seal
alongside `cfg.mountPoint`, with the same auto-seal + re-seal + doctor coverage).
It is deferred here, not rejected: it is additive (does not reopen Decision 1's
no-knob stance), but it is a real new public option with non-trivial scope (a
multi-path seal loop, per-path doctor coverage, and a correctness wrinkle the
static pool mountpoint does not have -- a `systemd.mounts` target dir may not exist
until first mount, so an offline-before-first-mount path reports `Absent` and is
sealed only once it exists). See the Plan-review fork in the revise report.

## Reconfiguration caveat (changing mountPoint)

braid seals and checks only the CURRENTLY configured `mount_point`. If an
operator changes `braid.mountPoint` (say `/mnt/storage` -> `/srv/storage`), the
`nixos-rebuild switch` that applies the change runs the seal oneshot for the NEW
path during that same activation (oneshot-on-switch -- Decision 3), so the new
path is sealed promptly. braid does NOT auto-clear the OLD one -- the old bare
directory keeps its `+i` until cleared, so a later `rmdir` or reuse of the old
path fails with `EPERM`. This is the same class as any NixOS path option (changing
`dataDir` leaves the old directory behind); braid does not track prior
mountpoints, consistent with the no-migration stance in AGENTS.md.

Remediation is the explicit-path clear lever, not `chattr` (absent from the
appliance wrapper PATH): `braid seal-mountpoint --unseal /mnt/storage`. The old
path is offline (not a mountpoint), so the fd guard clears it safely. This is
exactly the case the F4 refuse-configured rule permits: `--unseal` refuses only the
CURRENTLY configured `mount_point` (now `/srv/storage`), so clearing the OLD,
no-longer-configured `/mnt/storage` is allowed. The lock `--unseal` acquires also
means the clear cannot interleave with an `unlock` that would remount over the path
mid-operation. Document
this in the new decision doc and the README. (The doctor checks cannot surface
the orphaned old path -- without a recorded prior mountpoint they have nothing to
probe -- so discoverability is via docs and the `EPERM`-on-rmdir symptom, by
design.)

## Manual one-time verification (run once on the target kernel/btrfs)

```sh
mkdir /tmp/mp && chattr +i /tmp/mp
truncate -s 200M /tmp/img && mkfs.btrfs -q /tmp/img
mount -o loop /tmp/img /tmp/mp            # mount OVER the immutable dir
touch /tmp/mp/canary && echo "write into mounted fs: OK"
umount /tmp/mp
touch /tmp/mp/leak 2>/dev/null || echo "offline write rejected: EPERM (expected)"
lsattr -d /tmp/mp                         # expect ----i---------
chattr -i /tmp/mp && rmdir /tmp/mp && rm /tmp/img
```

## Test matrix (mapped to braid conventions)

Each test carries the 3-section preamble (Intent / Why it exists / Scenario) per
AGENTS.md.

### Rust unit tests (no root)

- `mountpoint_guard.rs`: `seal_offline_mountpoint` with `MockMountpointGuard` --
  asserts it always calls `enforce(true)`; every `GuardOutcome` maps to the right
  log path with no panic; in particular both `Unsupported` AND `MountStateUnknown`
  emit a WARNING (not a silent debug skip), so neither root-fs-unsupported nor
  old-kernel inertness can disable protection unnoticed; `Err` warns and returns
  (best-effort contract). This is the "decide only by mount state, abstracted from
  root" coverage the brief asks for.
- `mountpoint_guard.rs` `#[ignore]` smoke test (mirrors `btrfs_ioctl.rs:173`):
  `enforce` sets then clears `+i` on a tmp dir; returns `SkippedMounted` against a
  real mountpoint AND against a same-device **bind** mountpoint (the case an
  `st_dev`-only check would miss -- proves the `STATX_ATTR_MOUNT_ROOT` predicate).
- `mountpoint_guard.rs` non-directory guard (`#[cfg(target_os = "linux")]`, no root):
  `enforce` on a regular temp FILE returns `Ok(NotADirectory)` and sets no flags --
  pins that `O_DIRECTORY` refuses to seal a non-directory (a typo'd `braid.mountPoint`).
  Opening a regular file needs no privilege, so this is a fast non-ignored unit test,
  Linux-gated per the F1 target strategy (it exercises the real `open`).
- `mountpoint_guard.rs` ABI request-number const assertion (no root/VM): assert
  `nix::request_code_read!(b'f', 1, size_of::<FsFlagsArg>())` equals the kernel's
  `0x8008_6601` and `nix::request_code_write!(b'f', 2, size_of::<FsFlagsArg>())`
  equals `0x4008_6602`, reading the SAME `FsFlagsArg` alias the `ioctl_read!` /
  `ioctl_write_ptr!` invocations use. A regression flipping the alias to `c_int`
  yields `0x8004_6601` / `0x4004_6602` and fails in `just test-rust` -- the fast
  guard the mock tests cannot give (they bypass the real request number). VM case 1
  still proves the running kernel actually accepts the number; this catches the
  arithmetic regression without a VM. Gate it `#[cfg(target_os = "linux")]` (F1):
  the constants are Linux LP64, so on the `aarch64-darwin` `just test-rust` host the
  BSD-encoded macro would mismatch; it runs in the `x86_64-linux` lane, where a
  `c_long`->`c_int` regression would land.
- `mountpoint_guard.rs` non-Linux stub (`#[cfg(not(target_os = "linux"))]`, runs only
  on the `aarch64-darwin` host): assert `RealMountpointGuard::enforce(path, true)`
  returns `Ok(GuardOutcome::Unsupported)` and `is_immutable(path)` returns
  `Err(GuardError::Unsupported)`, and that feeding that `Err` through the doctor
  wrapper yields `ImmutabilityProbe::Indeterminate` ->
  `classify_mountpoint_immutability(_, Indeterminate) == None`. Pins that the
  cross-platform `just test-rust` host build links the stub (F1) and that it degrades
  to a clean no-finding -- never a false Warn/Failure, and never the type error
  `Ok(Unsupported)` would be for `is_immutable`.
- `doctor.rs`: pure `classify_mountpoint_immutability(mounted: Option<bool>, probe)`
  -- unit-test all branches: `Some(false)`+`Mutable` -> Warn, `Some(true)`+`Immutable` ->
  Failure, `Indeterminate` immutability (both mount states) -> None, mount-state
  `None` (any probe) -> None, otherwise -> None. Two cases are the ones a bare-bool
  signature could not express: `Indeterminate` immutability pins that an
  `Unsupported`/absent/old-kernel probe never produces the misleading "not immutable;
  reseal" Warn, and mount-state `None` pins that a FAILED mount probe (not a confirmed
  offline) suppresses both severities rather than firing a false offline+mutable Warn
  (F3). The VM root fs supports `+i`, so this
  branch is unreachable in the VM and this unit test is its ONLY coverage. No
  root/wiring needed (that is the point of the pure helper). The wiring around it
  -- check registered, probes the configured path via `guard.is_immutable`, maps
  to the right severity -- is covered behaviorally by the `braid doctor`
  assertions in VM cases 1 (no warning when sealed) and 8 (warning when
  offline+mutable); under boot-only this check is the sole non-boot detection
  signal, so the integration layer is worth pinning.
- `main.rs` `seal-mountpoint` dispatch (all three forms) with `MockMountpointGuard`:
  the bare form calls `enforce(mount_point, true)`; the explicit-path seal form
  `seal-mountpoint <path>` calls `enforce(<path>, true)` on the supplied path (not
  the configured one); the `--unseal <path>` form calls `enforce(<path>, false)`
  (always clears, never seals). Exit-code semantics (F2): only the bare boot form is
  always-0 (best-effort); BOTH explicit forms report honest desired-state exit codes.
  Drive the mock across outcomes and assert: explicit `seal-mountpoint <path>` exits 0
  on `Set` AND on `AlreadyImmutable`, and non-zero on `SkippedMounted` / `Absent` /
  `Unsupported` / `MountStateUnknown` / `NotADirectory` / `Err`; `--unseal <path>`
  exits 0 on `Cleared` AND on `AlreadyMutable` (a repeat unseal of an already-mutable
  path reports success, not failure), and non-zero on that same skipped/absent/
  unsupported/unknown/not-directory/error set; and `--unseal` REFUSES (hard error, no
  `enforce` call) when the supplied path equals the configured `mount_point`. Pins the
  symmetric supplied-path behavior, the F2 desired-state exit semantics for both
  explicit forms, AND the F4 refuse-configured contract. Also extend
  `lock_policy_classifies_every_command_and_branch` with the three argv forms --
  `["braid","seal-mountpoint"]` and `["braid","seal-mountpoint","<path>"]` -> `None`,
  `["braid","seal-mountpoint","--unseal","<path>"]` -> `NonBlocking` -- so the
  lock-acquiring split is pinned in the policy table.

### NixOS VM test -- `tests/module/immutable-mountpoint.{nix,py}`

Model on `tests/module/systemd-lifecycle.{nix,py}`; throwaway disks via
`virtualisation.emptyDiskImages`; add `pkgs.e2fsprogs` to `environment.systemPackages`
for `lsattr`/`chattr`. Register in `flake.nix` `checksFor`. Two nodes are required:
cases 1-8 and 10 run on a manual-unlock node (`autoUnlock` disabled -- pool offline
after boot, which cases 1/2/10 depend on); case 9 needs a SEPARATE node with
`autoUnlock.enable = true` + USB key present (pool online after boot), because
`autoUnlock` is a build-time NixOS config that cannot be toggled mid-test. Put
case 9 on a second node (e.g. `nodes.autoMachine`, modeled on
`tests/module/auto-unlock-key-present.nix`, with the USB-key `emptyDiskImages` /
keyfile scaffolding cases 1-8 do not need) in the same test file, or as a
separately registered test.

1. Offline immutable: after boot (the `braid-seal-mountpoint.service` seals while
   the pool is offline -- the seal does NOT come from unlock under Decision 3),
   `lsattr -d /mnt/storage` shows `i`; `machine.fail("touch /mnt/storage/x")`
   (EPERM); and `machine.fail("rmdir /mnt/storage")` -- the kernel refuses `rmdir`
   of an immutable dir (`may_delete` -> `IS_IMMUTABLE` -> `-EPERM`,
   `reference/linux/fs/namei.c`), the distinct "a sealed offline mountpoint cannot
   be silently removed and recreated mutable" property Decision 3 asserts but no
   case otherwise exercises (the write-EPERM above is a different property; case 8
   only `rmdir`s AFTER an explicit `--unseal`). Also assert `braid doctor` emits NO
   mutable-mountpoint warning in this
   boot-sealed state -- the positive half of the doctor wiring coverage (the check
   is registered, probes the configured path, and stays quiet when the invariant
   holds). The preamble names the VM's root filesystem (the fs backing
   `/mnt/storage`'s parent), since this case implicitly asserts that root fs
   supports `FS_IMMUTABLE_FL`; production support depends on the operator's
   unmanaged root fs, realistically unsupported only on non-NAS roots
   (vfat/9p/nfs) -- see the ADR note.
2. Online writable: `braid unlock`; `machine.succeed("touch /mnt/storage/x")` (write
   into the pool -- proves mount-over-immutable works).
3. Round-trip: starting from the boot-sealed offline dir -- unlock (writes OK) ->
   lock (same path rejects writes again; the seal set at boot persists across the
   cycle) -> unlock (writes OK; `stat -f`/`df` shows the write lands on the pool
   fs, not root).
4. Idempotency: unlock; lock; unlock again -- no error; `lsattr -d` still shows `i`
   while offline.
5. Safety assertion: after a normal online, `lsattr -d /mnt/storage` (now the
   MOUNTED pool root) shows NO `i` and writes succeed -- proves braid never sealed
   a live root.
6. Boot window + tmpfiles safety: boot WITHOUT unlocking ->
   `braid-seal-mountpoint.service` ran -> `lsattr -d /mnt/storage` shows `i`;
   `touch` fails (proves boot-time coverage). Then, with the dir still sealed and
   offline, run `systemd-tmpfiles --create` (or `systemctl restart
   systemd-tmpfiles-setup.service`) and assert it exits 0 with no `EPERM` against
   the mountpoint and the dir stays sealed. This is the only test that exercises
   the load-bearing "tmpfiles issues no chmod/chown against an already-sealed dir"
   claim: first-boot ordering seals AFTER `systemd-tmpfiles-setup` runs, so the
   service never sees a sealed dir during the test unless we re-run it -- yet on a
   real appliance every reboot after the first runs tmpfiles-setup against the
   persistent sealed dir. If the claim were false this case fails with a degraded
   `systemd-tmpfiles-setup.service`.

7. Bind-mount detection: while offline, `mount --bind` a scratch dir over
   `/mnt/storage`, run `braid seal-mountpoint`, and assert the bind-mount root did
   NOT receive `+i` (`lsattr -d` shows no `i`) -- proves `STATX_ATTR_MOUNT_ROOT`
   refuses to seal a same-device mount that an `st_dev` check would miss. (The
   `#[ignore]` smoke test covers this too but does not run in CI, so the guarantee
   needs a CI-run VM case.)
8. Out-of-band unseal detection + the `--unseal` remediation contract: start from
   the boot-sealed offline `/mnt/storage`, then simulate an OUT-OF-BAND unseal with
   raw `chattr -i /mnt/storage` (e2fsprogs is on the VM PATH; this is faithful -- the
   doctor Warn exists precisely to catch a mountpoint left mutable out-of-band, and
   `chattr` is that out-of-band path, absent from the appliance wrapper. Using the
   lever here is impossible by design: F4(b) makes `--unseal` REFUSE the configured
   path). In this offline+mutable window assert `braid doctor` DOES emit the
   mutable-mountpoint warning -- the negative half of the doctor wiring coverage
   (registered check, correct path, correct Warn severity at the integration layer,
   which the pure-helper unit test cannot reach). Then assert the F4 refuse-configured
   guard directly: `braid seal-mountpoint --unseal /mnt/storage` exits NON-ZERO and
   changes nothing (the live configured path may not be unsealed via the lever).
   `rmdir /mnt/storage` then succeeds (a cleared offline dir is removable). Finally
   exercise the lever's SANCTIONED use on a non-configured path: `mkdir /mnt/orphan`;
   `braid seal-mountpoint /mnt/orphan` sets `+i` (`lsattr -d` shows `i`);
   `braid seal-mountpoint --unseal /mnt/orphan` clears it (exit 0, `Cleared`,
   `lsattr -d` shows no `i`); a SECOND `--unseal /mnt/orphan` on the now-mutable path
   ALSO exits 0 (`AlreadyMutable`, no change -- the requested mutable state already
   holds, so the repeat is not a failure -- F2); and `--unseal` against a path that IS
   a mountpoint makes no change and exits non-zero (`SkippedMounted`) -- proving the
   lever clears an orphaned path, treats an already-cleared path as success, and
   refuses both the configured path and any live mount root.
9. Auto-unlock seal-before-mount ordering (the load-bearing boot path under
   Decision 3): on a DISTINCT node from cases 1-8 (see the two-node note above --
   `autoUnlock` is build-time, so it cannot share their machine) configure
   `braid.autoUnlock.enable = true` with the USB key PRESENT (model on
   `tests/module/auto-unlock-key-present.nix`), so the pool comes online at boot
   via `braid-auto-unlock.service` -- a system that never boots offline. Two
   assertions, one deterministic guard and one end-to-end sanity check:
   - **Deterministic edge guard (the actual regression guard):**
     `autoMachine.succeed("systemctl show -p After braid-auto-unlock.service | grep -q braid-seal-mountpoint")`.
     `After=` is the inverse of `Before=` (`reference/systemd/man/systemd.unit.xml`,
     "After= is the inverse of Before="), so systemd materializes the reverse edge on
     `braid-auto-unlock.service` whether the order is authored as the seal's `before`
     or auto-unlock's `after` (structure-insensitive). This holds regardless of the
     boot race and fails IFF a future refactor drops the edge.
   - **End-to-end sanity (complementary, NOT a standalone guard):** boot, confirm the
     pool is mounted (auto-unlock ran), then `braid lock` and assert
     `lsattr -d /mnt/storage` on the now-unmounted bare dir shows `i` and
     `autoMachine.fail("touch /mnt/storage/x")` -- the seal happened before the mount
     on a real boot. This cannot pin the edge ON ITS OWN: the seal is a single ioctl
     while auto-unlock does USB-read + cryptsetup + btrfs mount, so even WITHOUT the
     `before` edge the seal almost always wins the concurrent race and the dir ends up
     sealed -- the outcome stays green and silently hides a dropped edge. The
     deterministic assertion above is what catches that. (No other VM case enables
     `autoUnlock`.)
10. Activation self-heal + mounted-safety (pins the load-bearing "the seal oneshot
   re-runs on every `nixos-rebuild switch`/`test`" premise -- Decision 3 -- which every
   other case proves only at BOOT, never at activation). On the manual-unlock
   `machine` node (cases 1-8); needs only `e2fsprogs`, already present. A
   `switch-to-configuration test` re-runs the dead `Type=oneshot` seal unit because
   switch-to-configuration starts all active targets and systemd re-enqueues their
   `inactive (dead)` `Wants=` deps (nixpkgs `switch-to-configuration-ng` `main.rs`:
   the "start all active targets" loop + the active-`.target` restart branch).

   > **IMPLEMENTATION CORRECTION (do NOT copy the `switch-to-configuration` calls
   > below).** In the pinned nixpkgs the VM closure has NO `switch-to-configuration`
   > binary: this nixpkgs builds a separate/disabled activation, so the node
   > toplevel has only `activate`/`dry-activate` and no `bin/`, and the binary is
   > absent from the system path and the store -- so both `/run/current-system/bin/
   > switch-to-configuration` and `${toplevel}/bin/switch-to-configuration` fail with
   > exit 127. Re-implement both sub-cases against the unit directly: assert
   > `systemctl show -p WantedBy braid-seal-mountpoint.service` contains
   > `multi-user.target` (so a real `nixos-rebuild switch` re-enqueues the dead
   > `Wants=`), then `systemctl start braid-seal-mountpoint.service` to exercise the
   > dead-`Type=oneshot` re-run. Offline -> `ConditionPathIsMountPoint=!` met ->
   > re-seals (assert `i` + `touch` EPERM). Mounted -> condition false -> the start
   > is a no-op (still exits 0) -> live root stays mutable (assert no `i` + `touch`
   > succeeds). This proves braid's owned pieces; the switch re-enqueue itself is
   > standard systemd behavior. The `switch-to-configuration` steps below are the
   > original (broken-in-VM) sketch, kept for narrative context only. See
   > Implementation notes at the end.

   - **Offline self-heal:** ensure `/mnt/storage` exists (`mkdir -p`, since case 8
     `rmdir`s it) and is mutable via a raw OUT-OF-BAND `chattr -i /mnt/storage`
     (`lsattr -d` shows no `i`); then
     `machine.succeed("/run/current-system/bin/switch-to-configuration test")`; assert
     the activation RE-SEALED it -- `lsattr -d /mnt/storage` shows `i` and
     `machine.fail("touch /mnt/storage/x")` (EPERM). This is the ONLY case that proves
     the next-activation self-heal the doctor Warn defers to; without it, "the next
     activation re-seals" is asserted but never exercised.
   - **Mounted-safety:** `braid unlock` (pool mounts over the sealed dir; writes
     succeed -- `touch /mnt/storage/canary`), then run the SAME
     `switch-to-configuration test` while MOUNTED; assert the live pool root did NOT
     gain `+i` (`lsattr -d /mnt/storage` shows no `i`) and writes still succeed
     (`machine.succeed("touch /mnt/storage/x2")`). This pins that the unit's
     `ConditionPathIsMountPoint=!` plus the in-CLI `STATX_ATTR_MOUNT_ROOT` fd guard
     skip a mounted path during a mounted activation -- the exact "seal the live pool
     root during a mounted `nixos-rebuild switch`" failure mode a bare tmpfiles
     `chattr +i` hack would hit (Context) and that Decision 3 exists to avoid. `braid
     lock` afterward to leave the node offline-clean.

Edge cases: mountpoint dir absent -> `enforce` returns `Absent`, no crash (unit);
non-directory path -> `enforce` returns `NotADirectory`, warn (unit, Linux-gated per
F1); fs unsupported -> `enforce` returns `Unsupported`, warn (unit via mock, since the
VM root fs supports the attribute).

## Files to modify / add

- `modules/braid/storage.nix` -- new `braid-seal-mountpoint.service`, ordered
  `before = [ "braid-auto-unlock.service" ]` (the load-bearing seal-before-mount
  edge -- without it the seal races and loses on auto-unlock-with-USB systems;
  harmless no-op string when `autoUnlock` is disabled); the unit invokes the CLI via
  NixOS `path = [ braidWrapped ]; script = "braid seal-mountpoint";` (absolute
  generated-script `ExecStart`), matching `braid-unlock`/`braid-auto-unlock` -- never
  a relative `ExecStart = braid ...` (which systemd resolves against a compile-time
  fixed path, not the unit PATH -- `systemd.service.xml:1492`); tmpfiles rule
  unchanged (document why it is safe).
- `cli/src/mountpoint_guard.rs` -- NEW: trait, Real impl, mock, `seal_offline_mountpoint`,
  outcomes/errors (incl. `NotADirectory`), and the `FsFlagsArg = libc::c_long`
  buffer-type alias (used by both ioctl macro invocations and the ABI request-number
  const-assertion test); register in `cli/src/lib.rs`. The `statx`/`FS_IOC_*`
  `RealMountpointGuard` impl and the ABI const-assertion are
  `#[cfg(target_os = "linux")]`; a non-Linux `RealMountpointGuard` stub
  (`enforce -> Ok(GuardOutcome::Unsupported)`, `is_immutable -> Err(GuardError::Unsupported)`)
  so the crate builds and `just test-rust` runs the cross-platform half
  on the `aarch64-darwin` host (F1).
- `cli/src/mount.rs`, `cli/src/unlock.rs`, `cli/src/recover.rs`,
  `cli/src/test_fixtures/mount.rs`, `cli/src/pool.rs`, `cli/src/add.rs` --
  **NOT modified** (Decision 3, boot-only): the seal is neither in the bring-online
  mount path nor the create/bootstrap path, so `scan_and_mount`,
  `execute_mount_only` / `execute_unlock_and_mount`, `compile_open_steps`, the
  unlock/recover execute and preview arms (including
  `RecoverWorkAction::RemountCycle::render_into`), the shared mount test helpers,
  the `pool_bootstrap_mount` / `_raid1` fns, and `AddPlan::render_steps` are ALL
  untouched -- no `guard` parameter, no seal call, no preview step. This is the
  bulk of the churn the pivot removes: the feature never enters the mount, unlock,
  lock, recover, or create call graph.
- `cli/src/main.rs` -- `seal-mountpoint` subcommand (clap + dispatch): bare boot form
  (configured path) is lock-free `LockPolicy::None` and always exit 0 (best-effort);
  explicit-path seal `seal-mountpoint <path>` is lock-free `LockPolicy::None` but
  honest desired-state -- exit 0 iff `Set | AlreadyImmutable`, else non-zero (F2);
  `--unseal <path>` maintenance mode is the lock-acquiring remediation --
  `LockPolicy::NonBlocking`, refuses the configured `mount_point`, exit 0 iff
  `Cleared | AlreadyMutable`, else non-zero (F4 + F2). Pin every branch in
  `lock_policy_classifies_every_command_and_branch`.
- `cli/src/doctor.rs` -- pure `classify_mountpoint_immutability(mounted: Option<bool>,
  probe: ImmutabilityProbe)` helper + the `ImmutabilityProbe` enum + thin wrapper
  that maps `guard.is_immutable`'s `Result<bool, GuardError>` into the probe
  (`Err -> Indeterminate`) AND derives mount state as `Option<bool>` straight from
  `online_ops.is_mountpoint` (`Err -> None`, NOT via `ensure_mountpoint_is_mounted`'s
  `unwrap_or(false)`); the branch checks.
- `tests/module/immutable-mountpoint.{nix,py}` -- NEW VM test; register in `flake.nix`.
- Docs: new `docs/design/decisions/0NN-immutable-unmounted-mountpoint.md` (status
  Active), add to `docs/SUMMARY.md`; note the invariant in
  `docs/design/principles.md` if it rises to a principle; update `README.md` and
  `docs/commands`/`docs/guides` for the safety behavior, including the
  reconfiguration caveat and the `seal-mountpoint --unseal <path>` remediation.
  Update `docs/guides/mounting-subvolumes.md` for the Subvolume-mount caveat: state
  that the boot seal covers only the pool mountpoint, that subvolumes mounted UNDER
  `/mnt/storage` are protected by the parent seal, and that separate-path subvolume
  mounts retain the offline-write-to-root risk -- with the explicit-path
  `braid seal-mountpoint <path>` form as the braid-native manual remedy. The
  ADR must record:
  - The always-on tradeoff: the only capability lost is a declarative,
    rebuild-time off switch; recovery from any unforeseen interaction is the manual
    `--unseal` plus graceful self-disable (`Unsupported`/`MountStateUnknown`), not
    a NixOS option flip. The always-on default is reversible later under the
    no-backwards-compat policy if a real use case appears.
  - The seal-site decision (Decision 3) = boot-only, with the corrected rationale:
    the `braid-seal-mountpoint` oneshot runs on every boot AND every
    `nixos-rebuild switch`/`test`. Source this rather than assert it as bare
    "standard NixOS behavior": a `Type=oneshot` service without `RemainAfterExit`
    returns to `inactive (dead)` once `ExecStart` exits
    (`reference/systemd/man/systemd.service.xml`), and a `switch` (re)starts any
    unit `wantedBy` an active target that is not currently active -- nixpkgs
    `switch-to-configuration-ng` starts all active targets and systemd re-enqueues
    their `inactive (dead)` `Wants=` deps -- so the dead oneshot is started again on
    every activation (pinned behaviorally by VM case 10). The single static `cfg.mountPoint`
    is created by tmpfiles and sealed by that unit
    BEFORE any `braid add` runs, so the pool always mounts over an already-sealed
    dir and `+i` persists underneath. A create-time seal would be a redundant
    `AlreadyImmutable` no-op, so braid does NOT seal at create or in the mount
    path. Persistence carries the seal across unlock/lock cycles; the doctor
    "offline + mutable -> Warn" check plus the next-activation re-seal handle the
    rare out-of-band unseal.
  - The static-vs-dynamic mountpoint distinction, with the Rockstor precedent.
    Rockstor (a btrfs NAS) ships create-time sealing -- commit
    `5836560bbd1430c99fc73e3b6408fe3dcfd2220b`, "Make top level mount directories
    read-only when unmounted. Fixes #1414" -- BECAUSE its mountpoints are dynamic
    per-object `/mnt2/<name>` dirs born at creation with no boot-time existence to
    seal, and it has no boot re-seal. braid's single static mountpoint plus an
    activation/boot oneshot that fires before any create makes boot-only sufficient
    and create-time redundant; braid's boot re-seal also fixes Rockstor's fragility
    (create-only sealing never recovers from an out-of-band `chattr -i`). Cite
    Rockstor as real-world validation of the MECHANISM: its `bind_mount` does
    `mkdir` -> `chattr +i` -> `mount --bind` over the sealed dir
    (mount-over-immutable), and teardown does `chattr -i` -> `rmdir` (the kernel
    refuses `rmdir` of an immutable dir -- the same basis as braid's `--unseal`
    lever).
  - Revisit-if note: if braid ever moves away from the single static mountpoint
    (e.g. per-subvolume mounts at distinct root-fs paths, born on demand like
    Rockstor's), create-time sealing becomes necessary and this decision should be
    revisited.
  - The Subvolume-mount limitation (a CURRENT documented reality, not just the
    hypothetical in the revisit-if above): the boot seal covers only
    `cfg.mountPoint`, so the documented separate-path subvolume-mount pattern
    (`docs/guides/mounting-subvolumes.md`, `systemd.mounts` + `bindsTo`) retains the
    offline-write-to-root risk. This plan addresses it with documentation + the
    manual explicit-path `seal-mountpoint <path>` lever, NOT auto-sealing. Record
    the deferred fully-declarative escalation -- a `braid.extraSealedMountPoints`
    list the boot/activation oneshot would seal alongside `cfg.mountPoint` (additive,
    so it does NOT reopen Decision 1's no-knob stance) -- as a revisit-if, to be
    taken up if the manual lever proves insufficient. Note its correctness wrinkle:
    a `systemd.mounts` target dir may not exist until first mount, so an
    offline-before-first-mount extra path reports `Absent` until created, unlike the
    tmpfiles-created static pool mountpoint.
  - The external-writer behavior change (KEY AUDIT scope): any process writing the
    mountpoint while the pool is offline now fails with `EPERM` by design -- the
    intended loud failure replacing a silent write-to-root, called out so an
    operator whose backup/share service runs while offline understands the new
    `EPERM` is expected.
  - That `FS_IMMUTABLE_FL` support is effectively universal on real Linux roots
    (btrfs/ext4/xfs/f2fs/tmpfs all implement `.fileattr_set`); the `Unsupported`
    self-disable realistically fires only on non-NAS roots (vfat/9p/nfs), so it is
    a genuine but rare escape hatch, not a central rationale pillar.

## Verification (end to end)

1. `just test-rust` -- unit tests (guard logic + outcome->log mapping, the ABI
   request-number const assertion, the pure doctor classifier, and the
   `seal-mountpoint` dispatch across all three forms -- bare / `<path>` seal /
   `--unseal <path>`, including the F2 desired-state exit codes (explicit seal: exit
   0 iff `Set | AlreadyImmutable`; `--unseal`: exit 0 iff `Cleared | AlreadyMutable`;
   bare boot form always 0), the `--unseal` refuse-configured rule, and the per-branch
   lock-policy pins bare/`<path>`-seal=`None`, `--unseal`=`NonBlocking`). Runs on the
   `aarch64-darwin` host: the Linux-only `RealMountpointGuard` syscall code and the
   ABI assertion are `#[cfg(target_os = "linux")]` (non-Linux stub:
   `enforce -> Ok(GuardOutcome::Unsupported)`, `is_immutable -> Err(GuardError::Unsupported)`
   -> doctor `Indeterminate`/no finding), so the cross-platform half builds and runs on
   macOS while the real
   ioctl round-trip is proven in the Linux VM. No preview or bootstrap-call-site
   tests -- boot-only seals nothing inside a plan-and-execute command.
2. `just test-vm immutable-mountpoint systemd-lifecycle auto-unlock-key-present
   auto-unlock-key-missing` -- the new VM test (including case 9, the auto-unlock
   ordering regression), plus `systemd-lifecycle` and the `auto-unlock-*` family
   because the always-on `braid-seal-mountpoint.service` now joins the boot
   ordering graph of every auto-unlock config (it is unconditional, unlike
   `braid-auto-unlock.service` which is `mkIf cfg.autoUnlock.enable`). The seal is
   no longer in the shared mount path (Decision 3), so
   `subvol-mount-lifecycle`/`scrub-lifecycle` are no longer directly implicated by
   this change.
3. Run the manual loopback verification above once on the target kernel.
4. `mdbook build docs` -- cross-link check for the new decision doc / SUMMARY entry.
5. The remaining blast radius is the new boot unit (a systemd-lifecycle / boot
   ordering change), not the mount path: hand back for a full `just test-vm` run
   per the testing policy for systemd-lifecycle changes.

## Implementation notes

- The explicit-form exit semantics and the `--unseal` refuse-configured guard
  live in the LIB as `run_explicit_seal` / `run_explicit_unseal`
  (`cli/src/mountpoint_guard.rs`), not inline in `main.rs`. The plan asked for
  the dispatch to be tested "with `MockMountpointGuard`", but a lib's
  `#[cfg(test)]` mock is not visible to the binary crate's tests, so the tested
  logic had to be a lib function; `main.rs` is a thin `Result<String,String>` ->
  exit-code mapping over them. The bare boot form calls `seal_offline_mountpoint`
  directly.
- The ABI request-number guard is a `#[cfg(target_os = "linux")] #[test]`
  (`fs_ioc_flag_request_numbers_match_lp64_kernel_abi`) rather than a
  `const _: () = assert!(...)`. The plan explicitly permitted either form; the
  `#[test]` gives clearer failure diagnostics and runs in the `x86_64-linux`
  `just test-rust` lane (where a `c_long`->`c_int` regression would land).
- `classify_mountpoint_immutability` takes a `mount_point: &str` parameter the
  plan's signature sketch omitted, so the `Warn`/`Failure` messages embed the
  path while the helper stays fully unit-tested.
- The doctor check renders the pure helper's single `None` variant as `Ok` for
  the two healthy states (offline+immutable, online+mutable) and `Skip` for the
  indeterminate / mount-unknown rows, re-inspecting `(mounted, probe)` after the
  classifier. This keeps the severity decision (None/Warn/Failure) in the tested
  helper while avoiding a contradictory `[ok]` on an unsupported root.
- `mount_root_state` folds a total `statx` failure into `MountStateUnknown` for
  `ENOSYS`/`EINVAL` (fail closed, no flag change) and surfaces any other statx
  errno as `GuardError::Statx`; the plan specified only the mask-bit-unset path
  to `MountStateUnknown`.
- VM case 10 exercises the activation self-heal via
  `systemctl start braid-seal-mountpoint.service` plus a
  `WantedBy=multi-user.target` assertion, NOT `switch-to-configuration test` as
  the plan sketched. This nixpkgs builds a separate/disabled activation, so no
  `switch-to-configuration` binary exists in the VM closure (the toplevel has
  `activate`/`dry-activate` and no `bin/`). The systemctl re-run proves the
  braid-owned pieces -- the dead `Type=oneshot` re-runs, and the
  `ConditionPathIsMountPoint=!` gate skips a mounted root -- while the
  `WantedBy` assertion ties it to activation; the switch re-enqueue of dead
  `Wants=` is standard systemd behavior, not braid's to pin.
