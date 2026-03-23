# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. LUKS devices use `nofail` + bounded timeouts. btrfs-device-scan uses `wants`, not `requires`. The mount uses `nofail`. Degraded mounts require explicit `--allow-degraded` — braid refuses to silently run with zero redundancy. [Why →](decisions/resilient-boot.md)

## 2. Config-first workflow

Declare the disk in `braid.disks` (named attrset) before formatting it. `nixos-rebuild switch` exports config and creates LUKS entries. `braid add <disk>` formats and adds the disk. CLI tools refuse to operate on undeclared disks. Workflow: edit config → `nixos-rebuild switch` → `braid add <disk>`. [Why →](decisions/config-first-workflow.md)

## 3. Safe-by-construction operations

- `nixos-rebuild switch` is declarative and idempotent — always safe to run.
- Each intent command (`add`, `remove`, `remove-missing`, `replace`) does exactly one thing with risk-appropriate confirmation. `replace` always uses `btrfs replace start` — for live disks it replaces in-place, for missing disks it rebuilds from RAID redundancy using the missing device's devid. `remove-missing` cleans up a stale missing-device entry; it never rebuilds data onto a new device (that is `replace`). When clearing the last missing device with ≥2 devices remaining, both `remove-missing` and `replace` (missing path) run a follow-up soft balance to restore RAID1 profiles for chunks written during degraded operation.
- Disk names are immutable once recorded in braid state; name rename/reassignment is rejected by mutating commands and must use explicit `replace` or `remove`+`add` workflows.
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
