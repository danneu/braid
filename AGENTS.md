# AGENTS.md

## Project: braid

A NixOS-based NAS with full disk encryption, auto-healing storage, and dynamic drive pooling.

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
```

## The Stack

- **NixOS** — declarative, reproducible system configuration
- **LUKS** — passphrase-based full disk encryption (keys never stored on disk), SSH remote unlock via dropbear in initrd
- **btrfs RAID1** — checksumming filesystem with automatic self-healing from redundant copies; dynamic add/remove drives

## Architecture Authority

Design principles and invariants live in [`docs/principles.md`](docs/principles.md). Detailed rationale, rejected alternatives, and historical context live in [`docs/decisions/`](docs/decisions/).

Any change to behavior or invariants must update those docs. Code that contradicts a principle is wrong — fix the code or update the principle with rationale.

Decision docs must include an explicit status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

## User Guide

[`README.md`](README.md) is the end-user guide. Keep it updated when adding features or changing behavior. Style: brief, cookbook-like — short descriptions with copy-paste examples. Not reference material.

## References

- [User stories](docs/1-user-stories.md) — full UX walkthrough from first disk to third
- [Design: braid-add-disk](design-docs/1-braid-add-disk.md) — script design (historical, replaced by unified CLI)

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
- [x] Survive a drive failure — pool stays accessible in degraded mode
- [x] `braid-remove-disk` graceful remove (data migrates off, btrfs detach, LUKS close)
- [x] `braid-remove-disk` fallback remove-missing (device gone, `btrfs device remove missing`)
- [x] `braid-remove-disk` LUKS cleanup after btrfs remove
- [x] `braid-remove-disk` redundancy warning when dropping below 2 disks
- [x] `braid-status` summary output (drive count, RAID profile, capacity, health)
- [x] `braid-status --verbose` per-disk detail (model, serial, errors, LUKS UUID)
- [x] `braid init-disk` safety contract (declared-disk, pool-membership refusal, LUKS probe, force gate, passphrase check)
- [x] `braid plan` no-op, add, remove, replace, absent disk, JSON schema with status/warnings/blocked_reasons
- [x] `braid apply` add, remove, replace, checkpoint/resume, stale checkpoint refusal
- [x] `braid apply` safe-by-construction: no `luksFormat` reachable from apply path
- [x] `braid apply` redundancy confirmation when dropping below 2 disks
- [x] `braid apply` explicit missing-device removal gate (`--allow-remove-missing` + `BRAID_CONFIRM`)
- [x] `braid apply` absent disk skip+warn, unplug/replug regression
- [x] `braid status` human, `--json`, `--verbose` output
- [x] `braid doctor` config file check (exists, valid JSON, schema validation)
- [ ] LUKS header auto-backup on `init-disk`, corrupt header restore + data recovery
