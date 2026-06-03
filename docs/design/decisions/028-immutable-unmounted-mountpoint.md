---
intent: Record why braid seals the offline pool mountpoint immutable, why the
  seal lives only in the boot/activation unit, and why it is always-on. Read
  before changing the seal site, the seal unit ordering, or the maintenance
  levers.
status: Active
---

# Decision: Seal the offline pool mountpoint immutable

## Context

The pool mountpoint (default `/mnt/storage`) is a plain directory on the root
filesystem. When the pool is mounted there, writes go to the pool; when it is
NOT mounted, that bare directory is still writable, so any process writing under
the path silently lands data on the ROOT disk. When the pool later mounts over
it, that data is shadowed (invisible), permanently consumes root space, and the
write looked like it succeeded. This is the classic "unmounted mountpoint"
data-safety bug.

braid sets the inode immutable attribute (`FS_IMMUTABLE_FL`, a.k.a. `chattr +i`)
on the bare mountpoint directory while it is unmounted:

- Unmounted: a create/write under the directory fails immediately with `EPERM`.
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
reliably. A bare tmpfiles `chattr +i` hack could not: it would seal the live pool
root during a `nixos-rebuild switch` performed while the pool is mounted. braid's
unit gates on `ConditionPathIsMountPoint=!` and the in-CLI fd
`STATX_ATTR_MOUNT_ROOT` check, so it only ever seals the offline bare dir.

### Mechanism (verified against the pinned kernel)

- **Mount-over-immutable is allowed.** There is no `IS_IMMUTABLE` check in the
  kernel mount path (`reference/linux/fs/namespace.c`); the guard lives only in
  `fs/attr.c`. So the pool mounts over the sealed dir.
- **`+i` blocks metadata writes.** `may_setattr` (`reference/linux/fs/attr.c`)
  returns `-EPERM` for `chmod`/`chown`/explicit-time changes on an immutable
  inode -- the basis for the tmpfiles interaction below.
- **The kernel refuses `rmdir` of an immutable dir.** `may_delete` ->
  `IS_IMMUTABLE` -> `-EPERM` (`reference/linux/fs/namei.c`), so a sealed offline
  mountpoint cannot be silently removed and recreated mutable while offline.
- The fd-based mount-root check uses `statx`'s `STATX_ATTR_MOUNT_ROOT`, which is
  authoritative: unlike an `st_dev`-vs-parent comparison it also detects
  same-device and bind mountpoints (util-linux's own `mountpoint.c` notes its
  `st_dev` fallback "is ... not able to detect bind mounts").

## Decision

### 1. Always-on (non-configurable)

The seal is an unconditional safety invariant, in the same class as the baked-in
base mount options braid sets unconditionally -- `noatime`
([ADR 015](015-hdd-defaults.md)) and `skip_balance`
([Principles](../principles.md)). There is no `immutableWhenUnmounted` knob.

Rationale: there is no legitimate "off" use case (writing the bare offline
mountpoint *is* the bug). The escape hatches that matter -- graceful degradation
on an unsupported fs / old kernel (`Unsupported` / `MountStateUnknown`) and the
`braid seal-mountpoint --unseal <path>` lever -- exist independently of any flag.

**Tradeoff:** the only capability lost is a declarative, rebuild-time off switch.
Recovery from any unforeseen interaction is the manual `--unseal` plus the
graceful self-disable, not a NixOS option flip. Under the no-backwards-compat
policy the always-on default is reversible later if a concrete need ever surfaces
(a knob could be re-added trivially).

### 2. Close the boot window

A boot-time seal makes the invariant hold from boot, not only after the first
unlock. A NAS waiting for SSH unlock (auto-unlock off, or USB key absent --
`braid-auto-unlock.service` exits 0 on skip) otherwise sits offline-and-writable
indefinitely, and a unlock-path seal would never fire because nothing mounts.

### 3. Seal from the boot/activation unit ONLY

The seal lives in exactly one place: the `braid-seal-mountpoint` oneshot
(`modules/braid/storage.nix`). `braid add` does NOT seal, and neither does the
mount path. This is not a coverage gap -- a create-time seal would be a redundant
`AlreadyImmutable` no-op -- for two compounding reasons:

- **The oneshot runs on every activation, not just reboot.**
  `braid-seal-mountpoint.service` is `Type=oneshot` with no `RemainAfterExit`, so
  it returns to `inactive (dead)` once `ExecStart` exits
  (`reference/systemd/man/systemd.service.xml`). NixOS's
  `switch-to-configuration-ng` starts all active targets and systemd re-enqueues
  their `inactive (dead)` `Wants=` dependencies, so the dead oneshot is started
  again on every `nixos-rebuild switch`/`test` as well as every boot
  (self-healing). You cannot enable braid or change `braid.mountPoint` without an
  activation that runs the seal.
- **The mountpoint is static and pre-exists every pool.** `cfg.mountPoint` is a
  single fixed path created by the tmpfiles rule `d ${cfg.mountPoint}` on every
  boot/activation, so the seal unit seals it (while offline) BEFORE any
  `braid add` can run. The pool then mounts OVER the already-sealed dir; `+i`
  persists on the underlying inode, and braid's lock/unmount path never `rmdir`s
  or `chmod`/`chown`s the bare dir, so the next `braid lock` reveals it still
  sealed.

So any pool bootstrapped after braid is enabled inherits an already-sealed
mountpoint, and persistence carries the seal across every later unlock/lock with
no re-seal. The seal is NOT in the create/bootstrap path or the bring-online
mount path; the only seal call outside `braid seal-mountpoint` is the doctor's
read-only probe.

The `braid-seal-mountpoint` unit is ordered `before braid-auto-unlock.service`.
Both are pulled in by `multi-user.target`; without the edge they race, and if
auto-unlock won it would mount the pool and the seal unit's
`ConditionPathIsMountPoint=!` would then skip the seal. An auto-unlock-with-USB
NAS never boots offline, so without this edge nothing would ever seal the bare
dir. Ordering before auto-unlock runs the seal in the pre-mount window every
boot; auto-unlock then mounts over the sealed dir and persistence carries it.
When `autoUnlock` is disabled the unit does not exist and `before` is a harmless
no-op ordering string.

The doctor "offline + mutable -> Warn" check is the detection/self-heal signal
for the rare out-of-band unseal (e.g. a raw `chattr -i`); the next boot or
activation re-seals.

### Static-vs-dynamic mountpoint distinction (Rockstor precedent)

Rockstor (a btrfs NAS) ships **create-time** sealing -- commit
`5836560bbd1430c99fc73e3b6408fe3dcfd2220b`, "Make top level mount directories
read-only when unmounted. Fixes #1414" -- BECAUSE its mountpoints are dynamic
per-object `/mnt2/<name>` dirs born at creation with no boot-time existence to
seal, and it has no boot re-seal. braid's single static mountpoint plus an
activation/boot oneshot that fires before any create makes boot-only sufficient
and create-time redundant; braid's boot re-seal also fixes Rockstor's fragility
(create-only sealing never recovers from an out-of-band `chattr -i`).

Rockstor validates the MECHANISM: its `bind_mount` does `mkdir` -> `chattr +i` ->
`mount --bind` over the sealed dir (mount-over-immutable), and teardown does
`chattr -i` -> `rmdir` (the kernel refuses `rmdir` of an immutable dir -- the same
basis as braid's `--unseal` lever).

**Revisit-if:** if braid ever moves away from the single static mountpoint (e.g.
per-subvolume mounts at distinct root-fs paths, born on demand like Rockstor's),
create-time sealing becomes necessary and this decision should be revisited.

## Maintenance levers

`braid seal-mountpoint` is a visible command (`cli/src/main.rs`) with three forms
(`cli/src/mountpoint_guard.rs`):

- **`braid seal-mountpoint`** (no args) -- the bare boot/internal form. Seals the
  configured `mount_point`. Best-effort: it always exits 0 (a missing/inert guard
  must not block boot) and is lock-free. This is what the oneshot runs.
- **`braid seal-mountpoint <path>`** -- seal an explicit path. Lock-free, but
  reports an HONEST desired-state exit code: exit 0 iff the path ends up immutable
  (`Set` or `AlreadyImmutable`), non-zero otherwise. This is the remedy for
  separate-path subvolume mountpoints (below), where a silent best-effort exit 0
  would hide an unprotected path the doctor cannot see.
- **`braid seal-mountpoint --unseal <path>`** -- clear `+i` on an explicit path.
  Unlike the seal forms this is an operator remediation, not a boot action, so it
  (a) ACQUIRES the pool lock (fail-fast on contention), serializing against an
  in-flight `unlock`/`lock` so a concurrent mount cannot land the pool over a
  just-cleared bare dir; (b) REFUSES the currently configured `mount_point` (the
  live path must stay sealed while offline); (c) exits non-zero unless the path
  ends up mutable (`Cleared` or `AlreadyMutable`, so a repeat unseal of an orphan
  reports success).

All three forms route through the same fd-guarded `enforce`
(`cli/src/mountpoint_guard.rs#enforce`), which refuses any live mountpoint
(`SkippedMounted`) via `STATX_ATTR_MOUNT_ROOT`, so the levers only ever touch an
offline bare dir.

## Doctor detection

`braid doctor` is the sole non-boot detection signal under the boot-only model.
The pure classifier `cli/src/doctor.rs#classify_mountpoint_immutability` warns
when the pool is offline and the mountpoint is mutable (invariant not yet held --
self-seals on the next boot/activation, or run `braid seal-mountpoint`), and fails
when the pool is mounted and the inode is immutable (a live pool root was sealed
-- a tripwire that should never fire). Both the mount-state and immutability
inputs are tri-state, so a failed probe or an unsupported root suppresses the
finding rather than producing a misleading hint -- the seal unit owns the single
"protection unavailable" warning.

## Caveats

### External writers (intended behavior change)

This is a behavior change for operator-configured services, not a no-op. On a
NAS, services like Samba/NFS exports, Syncthing, Nextcloud, or cron/rsync backups
are routinely `wantedBy multi-user.target` and will write to `/mnt/storage` while
the pool is offline (auto-unlock skipped or USB absent, awaiting SSH unlock). With
`+i` those writes now fail with `EPERM`. That is the intended win: a loud `EPERM`
replaces the silent write-to-root that leaked space and got shadowed on mount. An
operator whose backup/share service runs while the pool is offline should expect
the new `EPERM`.

### Sole-mounter / fstab assumption

This invariant assumes braid is the only thing mounting the path. The module
replaced the `fileSystems` entry, so braid is the sole mounter by design -- there
is no fstab entry racing it. If an operator adds their own fstab line or mount
unit for the pool, external mount/unmount can bypass the seal and the invariant
can drift; the doctor check is the detection mechanism.

### Reconfiguration (changing `mountPoint`)

braid seals and checks only the CURRENTLY configured `mount_point`. If an operator
changes `braid.mountPoint` (say `/mnt/storage` -> `/srv/storage`), the
`nixos-rebuild switch` that applies the change runs the seal oneshot for the NEW
path during that same activation, so the new path is sealed promptly. braid does
NOT auto-clear the OLD one -- the old bare directory keeps its `+i` until cleared,
so a later `rmdir` or reuse of the old path fails with `EPERM`. This is the same
class as any NixOS path option (changing `dataDir` leaves the old directory
behind); braid does not track prior mountpoints, consistent with the
no-migration stance in `AGENTS.md`.

Remediation is the explicit-path clear lever (not `chattr`, which is absent from
the appliance wrapper PATH): `braid seal-mountpoint --unseal /mnt/storage`. The
old path is offline, so the fd guard clears it safely, and `--unseal` refuses only
the currently configured `mount_point` (now `/srv/storage`), so clearing the OLD,
no-longer-configured path is allowed. The doctor cannot surface the orphaned old
path (without a recorded prior mountpoint it has nothing to probe), so
discoverability is via this doc and the `EPERM`-on-rmdir symptom, by design.

### Separate-path subvolume mounts (not auto-sealed)

The boot seal covers ONLY `cfg.mountPoint`. braid documents and tests a pattern
([Mounting subvolumes](../../guides/mounting-subvolumes.md)) that mounts
subvolumes at SEPARATE root-fs paths -- e.g. `/var/lib/jellyfin/media` -- via
`systemd.mounts` with `bindsTo = braid-online.service`. When the pool is offline
those mount units are stopped, leaving bare root-fs directories at those paths,
so an undocumented writer there lands data on root -- the identical bug, NOT
covered by the boot oneshot (it seals one static path).

- **Subvolumes mounted UNDER the sealed `/mnt/storage`** are inherently protected
  by the parent seal and are the safe default.
- **Subvolumes mounted at separate paths** are an advanced, operator-opt-in
  pattern. This decision does NOT auto-seal them; it documents the limitation and
  points operators at the manual `braid seal-mountpoint <path>` lever (whose
  honest exit codes matter precisely because the doctor cannot see these paths).

The manual lever is honestly half-protective (not self-healing, and the doctor
cannot see these paths). **Revisit-if:** a fully-declarative
`braid.extraSealedMountPoints` list that the boot/activation oneshot would seal
alongside `cfg.mountPoint` (with the same auto-seal + re-seal + doctor coverage).
It is additive -- it does not reopen Decision 1's no-knob stance -- but it is a
real new public option with non-trivial scope (a multi-path seal loop, per-path
doctor coverage, and a correctness wrinkle the static pool mountpoint does not
have: a `systemd.mounts` target dir may not exist until first mount, so an
offline-before-first-mount path reports `Absent` until created). Deferred until
the manual lever proves insufficient.

### Filesystem support

`FS_IMMUTABLE_FL` is effectively universal on real Linux roots
(btrfs/ext4/xfs/f2fs/tmpfs all implement `.fileattr_set`). The `Unsupported`
self-disable realistically fires only on non-NAS roots (vfat/9p/nfs), so it is a
genuine but rare escape hatch, not a central rationale pillar. When it fires the
seal unit emits one clear "root filesystem does not support the immutable
attribute" warning, and the doctor stays quiet (it does not contradict that
signal with an un-actionable reseal hint).

## Dry-run / preview

Nothing to integrate. No braid plan-and-execute command seals the mountpoint, so
[ADR 022](022-dry-run-preview-model.md) imposes no obligation here: the seal is an
ambient systemd-unit-managed invariant (the same class as the tmpfiles
`d ${cfg.mountPoint}` rule), applied by the boot/activation oneshot outside the
plan/preview/execute model.

## See

- `modules/braid/storage.nix` -- the `braid-seal-mountpoint` oneshot.
- `cli/src/mountpoint_guard.rs` -- the guard, the seal site, and the maintenance levers.
- `cli/src/doctor.rs#classify_mountpoint_immutability` -- the detection signal.
- [ADR 018: Systemd lifecycle](018-systemd-lifecycle.md) -- the unit lifecycle model.
- [Mounting subvolumes](../../guides/mounting-subvolumes.md) -- the separate-path caveat.
