# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. The pool is unlocked and mounted by explicit CLI commands (`braid unlock` or `braid-auto-unlock`), not by systemd mount units. No LUKS or btrfs units are generated at build time. Degraded mounts require explicit `--allow-degraded` — braid refuses to silently run with zero redundancy. [Why →](decisions/resilient-boot.md)

## 2. CLI-owned membership

Disk membership is runtime state owned by the CLI, stored in `/var/lib/braid/pool.json`. Adding or removing a drive is `braid add name=/dev/disk/by-id/...` — no `nixos-rebuild` required. The NixOS module provides the mount point, services, and toolchain; the CLI owns which disks are in the pool. `unlock` requires `pool.json` to exist and be valid — it never creates or repairs it. Recovery is explicit via `braid discover --write`. [Why →](decisions/runtime-disk-membership.md)

## 3. Safe-by-construction operations

- Each intent command (`add`, `remove`, `remove-missing`, `replace`) does exactly one thing with risk-appropriate confirmation. `replace` always uses `btrfs replace start` — for live disks it replaces in-place, for missing disks it rebuilds from RAID redundancy using the missing device's devid. `remove-missing` cleans up a stale missing-device entry; it never rebuilds data onto a new device (that is `replace`). When clearing the last missing device with ≥2 devices remaining, both `remove-missing` and `replace` (missing path) run a follow-up soft balance to restore RAID1 profiles for chunks written during degraded operation.
- Post-commit persist with journal: mutating commands write a pending-operation journal (`pending-op.json`) with pre/target membership snapshots before the first irreversible disk operation. The journal is cleared only after the entire command — including required follow-up work like soft balance — succeeds. On any error exit, the journal persists so `braid recover` can reconcile. `pool.json` is written only after all disk operations succeed. If braid crashes or fails mid-operation, the journal triggers recovery mode — only `status`, `recover`, and `lock` are permitted. `braid recover` rebuilds membership from the live mounted pool (not LUKS label scanning).
- Disk names are immutable once recorded in pool membership; name rename/reassignment is rejected by mutating commands and must use explicit `replace` or `remove`+`add` workflows.
- `mkfs.btrfs` is gated on bootstrap only (no existing superblock).
- An existing LUKS device or pool member is never reformatted — a multi-layer identity check (LUKS label match, pool-mounted requirement, btrfs FSID comparison) prevents accidental data loss, with the btrfs superblock guard as defense-in-depth.
- Mounts always include `skip_balance` — btrfs silently resumes interrupted balances on mount by default, which can re-trigger ENOSPC or surprise the user with heavy I/O. braid manages balance lifecycle explicitly; `unlock` warns if a paused balance is detected.
- [Why →](decisions/intent-cli.md)

## 4. Single passphrase

All drives share one LUKS passphrase. `braid unlock` and `braid add` depend on this — one passphrase unlocks all drives. Enforced at format time: subsequent disks verify against an existing pool member via `cryptsetup --test-passphrase`. [Why →](decisions/single-passphrase.md)

Binary keyfile support is available via `braid enroll` (slot 1) and `braid.autoUnlock` (NixOS module). The passphrase (slot 0) remains the interactive-unlock mechanism; keyfiles are for unattended auto-unlock only.

## 5. Stable identifiers

All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are `braid-<disk-name>` (e.g., `braid-toshiba`) — deterministic, human-friendly, debuggable in `lsblk`, systemd logs, and error messages.
## 6. btrfs RAID1

Auto-healing checksums, dynamic drive pooling, in-kernel (no out-of-tree modules). 50% space overhead is accepted. btrfs RAID5/6 is not production-ready. [Why →](decisions/btrfs-raid1.md)

## 7. Sane defaults

If a knowledgeable admin would always enable it, braid enables it by default. Defaults use `lib.mkDefault` so users override with normal NixOS config. Only wrap in a `braid.*` option when the mapping is non-obvious or one braid option controls many underlying options. Examples: monthly auto-scrub, `storageGroup` for mount root access (`root:storage 2770`). [Why →](decisions/sane-defaults.md)

## 8. Test every design decision

NixOS VM tests validate behavior, not just command success. TDD: write failing tests first, confirm they fail for expected reasons, then implement.

## 9. NixOS-native

Braid only targets NixOS. No portability abstractions, no generic Linux fallbacks. Follow NixOS module conventions — same option types, patterns, and idioms as nixpkgs. When in doubt, nixpkgs is the tiebreaker. [Why →](decisions/nix-native.md)

## 10. Pinned toolchain

Parser-critical tools (btrfs-progs, cryptsetup, util-linux) are pinned to a specific NixOS stable release via the flake input. Wrappers execute with an explicit PATH built from module-controlled packages (`braid.packages.*`). Parsers assume the output format of the pinned version — upgrading those tools requires updating fixtures and parser tests. These pinned defaults are a compatibility baseline, not a lock; users may override `braid.packages.*` to pick up newer system versions when needed. Generic helpers (coreutils, systemd) come from the consumer's package set and are not part of braid's parser contract. [Why →](decisions/toolchain-pinning.md)

## 11. HDD defaults

Mount options, LUKS flags, and scrub scheduling are chosen for HDD NAS deployments. [Why →](decisions/hdd-defaults.md)

---

Implementation workflow and conventions are in [AGENTS.md](../AGENTS.md).
