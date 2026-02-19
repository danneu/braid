# AGENTS.md

## Project: btrnas

A NixOS-based NAS with full disk encryption, auto-healing storage, and dynamic drive pooling.

## Objective

Build a personal production NAS on NixOS using btrfs RAID1 for storage. The system should be encrypted, self-healing against bit rot, servable over SMB, and allow adding or removing drives without reformatting or migrating data.

Hardware target: 3x 12TB SATA 3.5" drives (1x Toshiba, 1x Ironwolf, 1x TBD), mini PC + DAS or NAS case.

## Architecture

```
Physical drives:
  /dev/sda → LUKS ─┐
  /dev/sdb → LUKS ─┼─ single btrfs RAID1 → /mnt/storage
  /dev/sdc → LUKS ─┘

Boot unlock:
  NAS powers on → initrd starts dropbear SSH + DHCP
  → ssh root@nas "cryptsetup-askpass" from MacBook
  → LUKS drives unlock → btrfs assembles → full boot continues

Samba:
  /mnt/storage → smb://nas/storage (LAN)
```

## The Stack

- **NixOS** — declarative, reproducible system configuration
- **LUKS** — passphrase-based full disk encryption (keys never stored on disk), SSH remote unlock via dropbear in initrd
- **btrfs RAID1** — checksumming filesystem with automatic self-healing from redundant copies; dynamic add/remove drives
- **Samba** — SMB file sharing (macOS, Windows, Linux)

## Why btrfs RAID1

- **Auto-healing bit rot** — checksums every block on every read, heals from the RAID1 copy automatically
- **Dynamic pool** — `btrfs device add` / `btrfs device remove` at any time, with any size drive
- **In-kernel** — no out-of-tree modules (unlike ZFS), first-class NixOS support
- **Incremental growth** — start with 1 drive (no redundancy), add a second to convert to RAID1, add more whenever
- **Simple stack** — LUKS + btrfs + Samba, three components total

## User Stories

See [`docs/1-user-stories.md`](docs/1-user-stories.md) — walks through the full experience from first disk to third, including `btrnas-add-disk` and the module's role.

## Design Principle: Resilient by Default

The OS lives on an internal SSD. The data drives are separate. Nothing about the data drives — bad config, dead drive, unplugged cable — should ever prevent the system from booting. The data pool is an external resource, like a network mount. The module tries to bring it up, but if it's not there, the box is still a working Linux machine you can SSH into and fix. The module should never be the reason you can't reach your NAS.

This means every module defaults to graceful failure, not hard failure:

- **LUKS devices**: `nofail` + short timeout. A missing drive times out and boot continues.
- **btrfs-device-scan**: `wants`, not `requires`. A failed cryptsetup doesn't cascade.
- **Mount**: `nofail` + `degraded`. A failed mount doesn't block boot. A partial pool still serves files.
- **Samba**: a broken share config shouldn't prevent boot.
- **Remote unlock (dropbear)**: a broken SSH config shouldn't prevent boot.

These are all no-ops when everything is healthy. Zero cost in the working case, graceful in every failure case. There is no `degraded` toggle or `nofail` option — resilience is the default, not an option.

Three tiers of failure, all graceful:

| Scenario                       | What happens                         | User sees                                |
| ------------------------------ | ------------------------------------ | ---------------------------------------- |
| All drives healthy             | Normal boot                          | Everything works                         |
| One drive dead                 | Short timeout, btrfs mounts degraded | Samba serves files, pool shows "missing" |
| All drives dead / wrong config | Short timeout, mount fails           | System boots, SSH works, no /mnt/storage |

## Design Principle: Module is Source of Truth

The NixOS config (`btrnas.disks`) declares which disks belong to the pool. The module exports this to `/etc/btrnas/config.json` at build time so runtime tools can read it. The workflow is config-first:

1. Add disk to `btrnas.disks`
2. `nixos-rebuild switch` — config exported, LUKS entries created (nofail if disk isn't formatted yet)
3. `btrnas-add-disk` — reads config, formats disk, adds to pool

CLI tools refuse to operate on disks not in the config. This prevents config drift (disk formatted but not in NixOS config) and ensures boot-time unlock works automatically.

## Key Tradeoffs

- **50% space overhead** — RAID1 stores 2 copies of every chunk. 3x 12TB = ~18TB usable. Parity schemes (SnapRAID, ZFS raidz) would give ~24TB, but btrfs RAID5/6 is not production-ready.
- **No drive independence** — drives are part of a btrfs pool, not individually mountable like ext4 + mergerfs. Recovery means having a working btrfs toolchain.
- **Rebalancing cost** — adding/removing a drive triggers a balance operation that can take hours on large pools.

## Commands

- `make test` — Run all NixOS VM tests.
- `make test-one t=<name>` — Run a single test by name (e.g. `make test-one t=hello-world`).
- `make test-one-verbose t=<name>` — Run a single test with full VM logs.
- `make test-verbose` — Run tests with full VM logs. Avoid unless debugging.

## Test Conventions

Every test file must start with a block comment explaining:

1. **What** is being tested
2. **Why** this test exists and what it validates in the architecture
3. **Dependencies** — what must already work for this test to be meaningful

## Development Approach: TDD with NixOS VM Tests

Write failing tests first, confirm they fail for the expected reasons, then implement the NixOS config to make them pass.

- **Test framework:** NixOS VM tests (`nixos/lib/testing-python.nix`)
- **Runs on macOS:** Requires `nix.linux-builder.enable = true` in nix-darwin. Tests are `checks.aarch64-darwin`.
- **Virtual disks:** `virtualisation.emptyDiskImages` creates throwaway virtual drives.

## Test Plan

- [x] VM boots with virtual drives present (hello-world)
- [x] LUKS encrypt + unlock virtual drives
- [x] Create btrfs RAID1 across LUKS devices, mount it
- [x] btrfs detects and heals corrupted data (write bad bytes, read back correct)
- [x] Add a new drive to existing btrfs RAID1 pool
- [x] Remove a drive from btrfs RAID1 pool
- [x] Start single-drive btrfs, convert to RAID1 after adding second drive
- [x] SSH remote unlock (dropbear in initrd, unlock from client VM)
- [x] Samba serves /mnt/storage, client VM mounts via SMB
- [x] Survive a drive failure — pool stays accessible in degraded mode
