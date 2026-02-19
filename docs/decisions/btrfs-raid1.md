# Decision: btrfs RAID1

> Principle: [btrfs RAID1](../principles.md#6-btrfs-raid1)

## Context

The NAS needs checksumming (bit rot detection), self-healing (automatic repair from redundant copy), and dynamic drive pooling (add/remove drives without reformatting). The filesystem sits on top of LUKS.

## Options considered

### ZFS raidz
- Checksumming + self-healing + RAID. Mature and well-tested.
- **Rejected**: out-of-tree kernel module. Licensing conflict means it can never be mainlined. NixOS supports it but it's a second-class citizen — kernel updates can break the module, and the build dependency is heavy.

### btrfs RAID5/6
- Same benefits as RAID1 with less overhead (parity instead of mirroring).
- **Rejected**: not production-ready. The write-hole bug has been a known issue for years. Data loss reports exist. The btrfs wiki explicitly warns against it.

### SnapRAID + mergerfs
- Parity-based protection with independent drives. ~75% space efficiency with 3+1.
- **Rejected**: no auto-healing. SnapRAID syncs on a schedule (e.g., nightly). Bit rot between syncs is undetected. No checksumming on read. Drives are independent ext4 — good for recovery but no real-time protection.

### btrfs RAID1
- Checksums every block on read, heals from the RAID1 copy automatically. Dynamic pool — `btrfs device add`/`remove` at any time with any size drive. In-kernel, first-class NixOS support. Simple stack: LUKS + btrfs + Samba.
- **Accepted**.

## Decision

btrfs RAID1. The 50% space overhead is accepted as the cost of real-time auto-healing with a simple, in-kernel stack.

## Tradeoffs accepted

- **50% space overhead** — 3x 12TB = ~18TB usable. Parity schemes would give ~24TB.
- **No drive independence** — drives are part of a btrfs pool, not individually mountable. Recovery requires a working btrfs toolchain.
- **Rebalancing cost** — adding or removing a drive triggers a balance operation that can take hours on large pools.
- **Incremental growth** — start with 1 drive (single profile, no redundancy), add a second to convert to RAID1. This is a feature, not a tradeoff — data is available immediately, protection comes when the second drive arrives.

## See

- `modules/btrnas/storage.nix` — btrfs mount configuration
- `tests/2-btrfs-heal.nix` — validates auto-healing
- `tests/3-btrfs-grow1.nix`, `tests/3-btrfs-shrink.nix` — validates dynamic pooling
