# Plan: Multi-disk `braid add`

## Context

`braid add` takes a single disk name. Starting a pool with two disks requires two `braid add` invocations — the second triggers a full RAID1 balance that rewrites all data. Multi-disk add lets users do `braid add disk1 disk2`, performing all LUKS work sequentially then one pool operation at the end.

For brand-new pools with 2+ disks, we use `mkfs.btrfs -d raid1 -m raid1 dev1 dev2 ...` to create the filesystem already in RAID1 — no balance needed. For existing pools, we add all devices then run one balance.

## Changes

### 1. `cli/src/main.rs` — Accept multiple disk names

`AddArgs.disk: String` → `disks: Vec<String>` with `num_args(1..)`.

Update `Commands::Add` doc comment: `/// Add a disk to the pool` → `/// Add disk(s) to the pool`.

Update call site to pass `&args.disks`.

### 2. `cli/src/cmd.rs` — New `MkfsBtrfsRaid1` variant

```rust
MkfsBtrfsRaid1 { devices: Vec<String> }
```

Generates: `mkfs.btrfs -f -d raid1 -m raid1 dev1 dev2 ...`

### 3. `cli/src/pool.rs` — New `pool_bootstrap_mount_raid1`

Takes `devices: &[String]`, calls `MkfsBtrfsRaid1`, then mounts `devices[0]`. No superblock check — callers must verify all devices are fresh before calling this function (the safety gate lives in `cmd_add`).

### 4. `cli/src/add.rs` — Refactor for multiple disks

`cmd_add` signature: `name: &str` → `names: &[String]`

New flow:

1. **Validate** — Look up each name in config. Reject duplicates upfront (dedup check before any probing).
2. **Probe** — Probe each disk. Fail early if any absent.
3. **Probe pool** + preflight (once).
4. **Compile steps** — Iterate all disks for dry-run display. Show one balance at the end (only for existing pool case).
5. **Read passphrase** — Once.
6. **Confirmation** — One message listing ALL disks to be LUKS-formatted.
7. **Verify passphrase** — Once against existing pool member (if pool exists).
8. **LUKS phase** — For each disk: format/open as needed. Track which need pool add. Skip disks already in pool.
9. **Pool phase** — `pool.mounted == false` does NOT mean brand-new pool (could be offline). Use `device_has_btrfs_superblock()` (already exists in `luks.rs`) to check each target mapper before deciding the path.
   - **Pool not mounted + 2+ disks + ALL fresh** (no superblocks on any target mapper): `pool_bootstrap_mount_raid1(all_devices)` — no balance needed.
   - **Pool not mounted + 1 disk**: `pool_bootstrap_mount(device)` — existing superblock check preserved (single-disk behavior unchanged).
   - **Pool not mounted + 2+ disks + ANY has superblock**: **reject with error.** Message: "Pool is not mounted but some target devices have existing btrfs data. Run `braid unlock` first to bring the pool online, then add disks." This is fail-closed — we don't attempt to guess which device to bootstrap from or whether the superblock belongs to the intended pool.
   - **Pool mounted**: `pool_add_device` for each, then one `pool_balance_raid1` if total >= 2.
10. **Finalize** — Update disk map for each added disk.

### 5. README.md

Update "Managing drives" section to show multi-disk usage:
- `braid add toshiba ironwolf` for starting with two disks at once
- Update dry-run example to show multi-disk output
- Keep single-disk examples working (backward compatible)

### 6. Tests

**Rust unit tests** (`cli/src/add.rs`, `cli/src/cmd.rs`):
- `MkfsBtrfsRaid1` generates correct argv
- Duplicate name rejection
- Multi-disk add rejects when pool is offline and any target mapper has a btrfs superblock (guards the fail-closed safety path)
- Update `add_confirm_message` test for new signature

**VM test** (new file `tests/module/multi-add.nix` or extend existing):
- Add 2 disks to empty pool in one command → verify RAID1 from the start, no balance needed
- Start with 1-disk pool, then `braid add disk2 disk3` → verify final pool health, RAID1 profile, data integrity across all 3 devices
- Add 1 disk to existing pool → verify backward compat

## Files to modify

- `cli/src/main.rs` — AddArgs, call site
- `cli/src/cmd.rs` — new `MkfsBtrfsRaid1` variant + `to_argv`
- `cli/src/pool.rs` — new `pool_bootstrap_mount_raid1`
- `cli/src/add.rs` — refactor `cmd_add`, `compile_add_steps`, `add_confirm_message`
- `README.md` — multi-disk examples

## Verification

1. `just test-rust` — Rust unit tests
2. `just test` — full VM test suite (existing tests validate single-disk backward compat)
