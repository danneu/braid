# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. LUKS devices use `nofail` + bounded timeouts. btrfs-device-scan uses `wants`, not `requires`. The mount uses `degraded` + `nofail`. There is no toggle — resilience is the default, not an option. [Why →](decisions/resilient-boot.md)

## 2. Config-first workflow

Declare the disk in `braid.disks` before formatting it. `nixos-rebuild switch` exports config and creates LUKS entries. `braid init-disk` formats the disk with LUKS (explicit, one-shot). `braid apply` handles non-destructive reconciliation. CLI tools refuse to operate on undeclared disks. [Why →](decisions/config-first-workflow.md)

## 3. Safe-by-construction reconciliation

`nixos-rebuild switch` is declarative and idempotent — always safe to run. `braid apply` is safe to run repeatedly — it never performs destructive disk initialization. Only `braid init-disk` performs LUKS formatting, and it requires explicit operator intent. `cryptsetup luksFormat` is forbidden in the plan/apply code path. [Why →](decisions/safe-by-construction-reconciliation.md)

## 4. Single passphrase

All drives share one LUKS passphrase. Remote unlock depends on this — one passphrase unlocks all drives. Enforced at format time: subsequent disks verify against an existing pool member via `cryptsetup --test-passphrase`. [Why →](decisions/single-passphrase.md)

## 5. Stable identifiers

All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are derived from by-id basenames so module and script stay deterministic. Kernel device names are unstable across reboots and hardware changes. [Why →](decisions/mapper-naming.md)

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
