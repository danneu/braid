# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. LUKS devices use `nofail` + bounded timeouts. btrfs-device-scan uses `wants`, not `requires`. The mount uses `degraded` + `nofail`. There is no toggle — resilience is the default, not an option. [Why →](decisions/resilient-boot.md)

## 2. Config-first workflow

Declare the disk in `braid.disks` (named attrset) before formatting it. `nixos-rebuild switch` exports config and creates LUKS entries. `braid add <disk>` formats and adds the disk. CLI tools refuse to operate on undeclared disks. Workflow: edit config → `nixos-rebuild switch` → `braid add <disk>`. [Why →](decisions/config-first-workflow.md)

## 3. Safe-by-construction operations

- `nixos-rebuild switch` is declarative and idempotent — always safe to run.
- Each intent command (`add`, `remove`, `remove-missing`, `replace`) does exactly one thing with risk-appropriate confirmation. `replace` handles both live and dead/missing old disks with add-first ordering.
- Disk names are immutable in v1.0 once recorded in braid state; name rename/reassignment is rejected by mutating commands and must use explicit `replace` or `remove`+`add` workflows.
- `mkfs.btrfs` is gated on bootstrap only (no existing superblock).
- An existing LUKS device or pool member is never reformatted — the btrfs superblock guard prevents accidental data loss.
- [Why →](decisions/intent-cli.md)

## 4. Single passphrase

All drives share one LUKS passphrase. `braid unlock` and `braid add` depend on this — one passphrase unlocks all drives. Enforced at format time: subsequent disks verify against an existing pool member via `cryptsetup --test-passphrase`. [Why →](decisions/single-passphrase.md)

Binary keyfile support is available via `braid enroll` (slot 1) and `braid.autoUnlock` (NixOS module). The passphrase (slot 0) remains the interactive-unlock mechanism; keyfiles are for unattended auto-unlock only.

## 5. Stable identifiers

All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are `braid-<disk-name>` (e.g., `braid-toshiba`) — deterministic, human-friendly, debuggable in `lsblk`, systemd logs, and error messages. [Why →](decisions/mapper-naming.md)

## 6. btrfs RAID1

Auto-healing checksums, dynamic drive pooling, in-kernel (no out-of-tree modules). 50% space overhead is accepted. btrfs RAID5/6 is not production-ready. [Why →](decisions/btrfs-raid1.md)

## 7. Sane defaults

If a knowledgeable admin would always enable it, braid enables it by default. Defaults use `lib.mkDefault` so users override with normal NixOS config. Only wrap in a `braid.*` option when the mapping is non-obvious or one braid option controls many underlying options. [Why →](decisions/sane-defaults.md)

## 8. Test every design decision

NixOS VM tests validate behavior, not just command success. TDD: write failing tests first, confirm they fail for expected reasons, then implement.

## 9. NixOS-native

Braid only targets NixOS. No portability abstractions, no generic Linux fallbacks. Follow NixOS module conventions — same option types, patterns, and idioms as nixpkgs. When in doubt, nixpkgs is the tiebreaker. [Why →](decisions/nix-native.md)

## 10. Pinned toolchain

Runtime tool versions are pinned to a specific NixOS stable release via the flake input. Both shell and Rust wrappers execute with an explicit PATH containing only module-controlled packages. Parsers assume the output format of the pinned version. Upgrading tools requires updating golden-file fixtures and parser tests. [Why →](decisions/toolchain-pinning.md)

---

Implementation workflow and conventions are in [AGENTS.md](../AGENTS.md).
