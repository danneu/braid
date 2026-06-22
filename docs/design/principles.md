---
intent: Canonical braid invariants. Read before changing behavior, safety model, storage lifecycle, CLI command semantics, or module defaults.
---

# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. The pool is unlocked and mounted by explicit CLI invocations (`braid unlock`, the `braid-auto-unlock.service` unit, or `braid recover` during recovery), not by systemd mount units. No LUKS or btrfs units are generated at build time. Degraded mounts require explicit `--allow-degraded` — braid refuses to silently run with zero redundancy. [Why →](decisions/003-resilient-boot.md)

## 2. CLI-owned membership

Disk membership is runtime state owned by the CLI, stored in `/var/lib/braid/pool.json`. Adding or removing a drive is `braid add name=/dev/disk/by-id/...` — no `nixos-rebuild` required. The NixOS module provides the mount point, services, and toolchain; the CLI owns which disks are in the pool. `unlock` requires `pool.json` to exist and be valid — it never creates or repairs it. Recovery is explicit via `braid discover --write`. [Why →](decisions/017-runtime-disk-membership.md)

`pool.json` is a best-effort operational snapshot — it tells braid which drives to attempt unlocking, not what the pool actually looks like. Any state that can be read from live btrfs (devids, device counts, FSID) must come from btrfs, not pool.json. Commands like `status` must never surface pool.json-sourced devids; **for display authority**, devids are authoritative only when read from a mounted filesystem via `btrfs device usage` or equivalent. **Persisted `DiskMember.devid` carries prior-binding authority only**: when live btrfs reports a device by `devid` alone (the `null_underlying` mapper case and the btrfs `missing_devids` case), the persisted `devid` is the authorized fallback binding for re-attaching that live device to its membership entry. This is not a display-side use of pool.json devid; status output continues to draw devids from live btrfs. [Why →](decisions/024-luks-uuid-identity.md)

## 3. Safe-by-construction operations

- Each intent command (`add`, `remove`, `remove-missing`, `replace`) does exactly one thing with risk-appropriate confirmation. `replace` always uses `btrfs replace start` — for live disks it replaces in-place, for missing disks it rebuilds from RAID redundancy using the missing device's devid. `remove-missing` cleans up a stale missing-device entry; it never rebuilds data onto a new device (that is `replace`). When clearing the last missing device with ≥2 devices remaining, both `remove-missing` and `replace` (missing path) run a follow-up soft balance to restore RAID1 profiles for chunks written during degraded operation.
- Post-commit persist with journal: mutating commands write a pending-operation journal (`pending-op.json`) with pre/target membership snapshots before the first irreversible disk operation. `pool.json` is written once the btrfs membership change has committed, so it reflects committed live membership, not necessarily completion of follow-up maintenance such as RAID1 rebalance or resize. Phased journals advance to post-maintenance after the committed `pool.json` write; those post phases must never rerun the primary btrfs membership mutation. The journal is cleared only after the entire lifecycle succeeds, including required post-mutation maintenance like soft balance. While the journal exists, `braid recover` replays owed maintenance when btrfs balance state is idle and fails closed with the journal preserved when owed RAID1 replay finds a crash-paused, running, or unknown balance state. If braid crashes or fails mid-operation, the journal triggers recovery mode: membership/mount/key-enrollment commands (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `enroll`, `discover --write`) hard-fail; read-only diagnostic and cleanup surfaces (`status`, `doctor`, `lock`, bare `discover`) stay available. `braid recover` rebuilds membership from the live mounted pool (not LUKS label scanning) and is the only command that clears the journal.
- Environment-side resource acquisition (file locks, sleep inhibitors, dbus/logind handshakes, external service availability) must happen **before** `journal::write_journal`. The journal write commits the user to recovery mode on any subsequent failure, so a pure environment failure (logind unreachable, flock contention) must not leave a stranded `pending-op.json` for what was conceptually a "command never started" failure. The journal write is the line of no return; reorder code so any RAII guards or environment probes that can fail are bound above it. The per-command pre-journal excluded scope (which also covers reversible validation and identity checks) is enumerated in [ADR 019](decisions/019-inhibit-sleep.md#current-application).
- Disk names are immutable once recorded in pool membership; name rename/reassignment is rejected by mutating commands and must use explicit `replace` or `remove`+`add` workflows.
- `mkfs.btrfs` is gated on bootstrap only -- bootstrap accepts only disks classified as fresh non-LUKS during add planning, and the LUKS open helpers verify that any pre-existing `braid-<name>` mapper is backed by the requested by-id disk before pool creation proceeds. `mkfs.btrfs` is invoked without `-f` so its own libblkid signature check is the final fail-closed guard.
- An existing LUKS device or pool member is never reformatted — a multi-layer identity check (LUKS label match, LUKS UUID cross-check against pool.json, pool-mounted requirement, btrfs FSID comparison) prevents accidental data loss, with the btrfs superblock guard as defense-in-depth.
- Failed `unlock` and recovery mount paths close only LUKS mappers braid newly opened during that invocation. They never close pre-existing operator-owned mappers, including mappers that become already open between planning and execution.
- Mounts always include `skip_balance` -- btrfs silently resumes interrupted balances on mount by default, which can re-trigger ENOSPC or surprise the user with heavy I/O. braid manages balance lifecycle explicitly; `unlock` warns if a paused balance is detected. Pool mounts also include `nosuid,nodev` for shared-pool privilege containment. [Why ->](decisions/032-pool-mount-hardening.md)
- The bare pool mountpoint is sealed immutable (`chattr +i`) while the pool is offline, so a process writing it before mount fails with `EPERM` instead of silently landing on the root filesystem and being shadowed when the pool mounts. The seal is always-on (no knob), lives only in the boot/activation unit (`braid-seal-mountpoint`), and persists across lock/unlock. `braid.mountPoint` is constrained at eval to a canonical path with no whitespace or shell metacharacters, so shell, tmpfiles, systemd, JSON, and CLI consumers interpolate it unquoted by construction. [Why →](decisions/028-immutable-unmounted-mountpoint.md)
- Dry-run previews for migrated mutating commands are rendered from the same typed work plans that execution consumes; `Step` is output-only. [Why ->](decisions/022-dry-run-preview-model.md)
- [Why →](decisions/012-intent-cli.md)

## 4. Single passphrase

All drives share one LUKS passphrase. `braid unlock` and `braid add` depend on this — one passphrase unlocks all drives. Before any irreversible operation, every reachable existing LUKS device that will remain in or enter post-operation pool membership has its slot 0 verified. Fresh-format disks are excluded because they have no existing slot 0. The live-replace source is excluded when other retained members exist, so a divergent slot 0 on the disk being replaced does not block its own replacement. The same all-relevant-disk rule applies to keyfile credentials used by `mount`, `unlock`, and `recover`. [Why →](decisions/004-single-passphrase.md)

Binary keyfile support is available via `braid enroll` (slot 1) and `braid.autoUnlock` (NixOS module). The passphrase (slot 0) is the default interactive-unlock mechanism; the slot-1 keyfile drives `braid.autoUnlock` for unattended boots and can also be passed directly to `braid unlock --key-file`.

## 5. Stable identifiers

All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are `braid-<disk-name>` (e.g., `braid-toshiba`) — deterministic, human-friendly, debuggable in `lsblk`, systemd logs, and error messages. **`LuksUuid` is the primary persistent identity for code; the disk name and the LUKS label are presentation; `by_id` is for hardware addressing. When the live LUKS UUID is unobservable for a device the kernel/btrfs still reports (`null_underlying` mapper, btrfs `missing_devids`), btrfs `devid` is the only authorized live-fallback binding key. No code path may decide membership, target a device, or correlate live pool state by parsing a name out of a mapper path or LUKS label, except in two narrow cases: `discover` bootstrapping a UUID-keyed membership from cold disks, and returning-disk adoption safety in `add` (the `PresentLuks` path may gate adoption on label match, but identity correlation still uses `LuksUuid`/`devid`/FSID).** [Why →](decisions/024-luks-uuid-identity.md)
## 6. btrfs RAID1

Auto-healing checksums, dynamic drive pooling, in-kernel (no out-of-tree modules). 50% space overhead is accepted. btrfs RAID5/6 is not production-ready. [Why →](decisions/001-btrfs-raid1.md)

## 7. Sane defaults

If a knowledgeable admin would always enable it, braid enables it by default. Use `lib.mkDefault` for simple pass-through defaults on stable NixOS options. Wrap in a `braid.*` option when the feature is inside braid's product boundary and benefits from lifecycle control, discoverability, or a unified config surface -- even if the mapping is 1:1. Examples: `braid.autoScrub` (periodic scrub with lifecycle binding to pool online state), `poolAccessGroup` for mount root access (`root:storage 2770`). [Why ->](decisions/005-sane-defaults.md)

## 8. Test every design decision

NixOS VM tests validate behavior, not just command success. TDD: write failing tests first, confirm they fail for expected reasons, then implement.

## 9. NixOS-native

Braid only targets NixOS. No portability abstractions, no generic Linux fallbacks. Follow NixOS module conventions — same option types, patterns, and idioms as nixpkgs. When in doubt, nixpkgs is the tiebreaker. [Why →](decisions/006-nix-native.md)

## 10. Pinned toolchain

Parser-critical tools (btrfs-progs, cryptsetup, util-linux, NUT, smartmontools, ethtool) are pinned to a specific NixOS stable release via the flake input. Wrappers execute with an explicit PATH built from module-controlled packages (`braid.packages.*`). Parsers assume the output format of the pinned version -- upgrading those tools requires updating fixtures and parser tests. These pinned defaults are a compatibility baseline, not a lock; users may override `braid.packages.*` to pick up newer system versions when needed. Generic helpers (coreutils, systemd) come from the consumer's package set and are not part of braid's parser contract, except that Browse parses `systemctl list-units --output=json` as a tolerant UI-only picker with raw-output fallback. [Why ->](decisions/010-toolchain-pinning.md)

## 11. HDD defaults

Mount options, LUKS flags, and scrub scheduling are chosen for HDD NAS deployments. [Why →](decisions/015-hdd-defaults.md)

## 12. One pool operation at a time

Rust dispatch acquires `/run/braid-pool.lock` before loading config, loading `pool.json`, probing pool state, or prompting for command input. The authoritative command-to-lock-discipline mapping lives in `lock_policy` in `cli/src/main.rs`; its wildcard-free exhaustive match makes every `Commands` variant choose a discipline at compile time.

Lock disciplines are policy categories, not prose-maintained command lists. Interactive mutators acquire non-blocking and fail fast with `braid: another braid operation is already in progress` so the user can retry once the active operation completes. Short-contention maintenance paths may wait for a bounded timeout, such as the 10-second alert acknowledgement window. Timer-driven monitoring may exit 0 silently on contention because a missed cycle is harmless and exit 1 would falsely start alert notification. Read-only paths and dry-run modes do not acquire the lock; bare `discover` is read-only, while its write mode participates because the scan -> `pool.json` write window must be serialized against pool-state mutators. Read-only diagnostics `status` and `doctor` never acquire the lock so operators retain a working diagnostic surface during contention; `tests/module/pool-lock-readonly-bypass.py` pins this invariant.

Mutual exclusion is enforced at the critical section itself, not via systemd unit topology. Under the held lock, `unlock` re-checks whether the pool is already mounted and exits cleanly if a prior winner mounted it sequentially; other mutators operate on the current locked state rather than stale pre-lock observations.

## 13. Announce long-running work

Every interactive command emits a `[wait]` row before any subprocess
that can stall the terminal long enough for the user to wonder
whether the CLI has hung. The bound categories:

- cryptsetup Argon2 operations (`luksFormat`, `luksOpen`,
  `luksAddKey`, `--test-passphrase`);
- `cryptsetup close` (single attempt or busy-retry loop);
- btrfs `balance`, `replace`, and `device remove` (potentially hours);
- `mount` and `umount` (kernel can drain in-flight I/O / replace
  workers / inhibitors).

A `[wait]` row is closed by one of:

- the same command's paired success row (`[ok]   {same subject}: ...`)
  on the success path,
- a same-subject `[fail]` row on a known failure path (e.g.
  `lock.rs`'s umount failure),
- a same-subject `[warn]` row on a non-fatal best-effort failure
  (e.g. `mapper_close::close_mapper_best_effort`'s LUKS close, or
  `wait_for_kernel_replace_to_finish`'s status-poll error — the
  command continues despite the failure, and the warn row tells
  the user the wait window is closed without success),
- a same-subject `[skip]` row on a successful negative or no-op
  probe (e.g. `braid enroll`'s pre-mutation keyfile probe finding
  the keyfile not yet enrolled — the work the wait announced
  completed, the answer is "no work yet"),
- or the command's normal error output (`MountError` / `LuksError` /
  `PoolError` propagation) on uncaught error paths.

A `[wait]` followed by none of these closers (i.e., success, fail,
warn, skip, or non-zero exit) is a documentation bug.

Fast bookkeeping that completes well under a second
(`mkfs.btrfs` on a fresh disk, `btrfs device add`,
`btrfs filesystem resize`, `btrfs device scan`,
`btrfs device scan --forget`, `cryptsetup luksHeaderBackup`,
`cryptsetup status`, `blkid`, JSON parses, journal writes,
`pool.json` saves, sysfs reads) does not warrant a row.

Rendering uses `status_tag::status_line(StatusTag::Wait, ...)`
against `color_enabled_for_stderr()` so plain stderr captures
contain unwrapped `[wait]` bytes and TTY output picks up the gray
ANSI tag. [Why →](decisions/021-wait-in-unlock.md)

## 14. Explicit subprocess environment

Every child process the braid Rust CLI binary spawns runs with an explicit
environment allowlist: parent PATH forwarded when present, plus `LC_ALL=C`.
The CLI never passes its inherited environment through to a child. Nix-generated
unit scripts and wrappers are out of scope; their environment is set by the
systemd unit and the wrapper. [Why ->](decisions/034-subprocess-environment-discipline.md)

---

Implementation workflow and conventions are in `AGENTS.md` at the repo root.
