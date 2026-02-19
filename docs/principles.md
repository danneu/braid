# Principles

Canonical invariants for braid. Each principle is authoritative — if code or config contradicts a principle, the code is wrong.

## 1. Resilient by default

Data drives never block boot. LUKS devices use `nofail` + bounded timeouts. btrfs-device-scan uses `wants`, not `requires`. The mount uses `degraded` + `nofail`. There is no toggle — resilience is the default, not an option. [Why →](decisions/resilient-boot.md)

## 2. Config-first workflow

Declare the disk in `braid.disks` before formatting it. `nixos-rebuild switch` exports config and creates LUKS entries. `braid-add-disk` reads config, formats, and joins the pool. CLI tools refuse to operate on undeclared disks. [Why →](decisions/config-first-workflow.md)

## 3. nixos-rebuild never destroys data

`nixos-rebuild switch` is declarative and idempotent — always safe to run. Only `braid-add-disk` performs destructive operations (LUKS format, btrfs create/add), and it requires explicit confirmation.

## 4. Single passphrase

All drives share one LUKS passphrase. Remote unlock depends on this — one passphrase unlocks all drives. Enforced at format time: subsequent disks verify against an existing pool member via `cryptsetup --test-passphrase`. [Why →](decisions/single-passphrase.md)

## 5. Stable identifiers

All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are derived from by-id basenames so module and script stay deterministic. Kernel device names are unstable across reboots and hardware changes. [Why →](decisions/mapper-naming.md)

## 6. btrfs RAID1

Auto-healing checksums, dynamic drive pooling, in-kernel (no out-of-tree modules). 50% space overhead is accepted. btrfs RAID5/6 is not production-ready. [Why →](decisions/btrfs-raid1.md)

## 7. Test every design decision

NixOS VM tests validate behavior, not just command success. TDD: write failing tests first, confirm they fail for expected reasons, then implement.

---

Implementation workflow and conventions are in [AGENTS.md](../AGENTS.md).
