# Plan: Implement `braid-status`

## Context

`braid-status` is a read-only diagnostic tool for the braid NAS. The spec is in `docs/decisions/disk-pool-management.md` (lines 68-80). It reports pool health (default) and per-disk detail (`--verbose`). No mutations, no passphrase, no confirmation. The project uses TDD — write failing tests first, then implement.

## Proposed output format

The spec defines *what* fields to show but not *how*. Proposed format:

### Default — healthy 3-disk RAID1

```
Pool:     /mnt/storage
Status:   healthy
Drives:   3
Profile:  RAID1

Capacity:
  Total:  768.00 MiB
  Used:   128.00 KiB
  Free:   255.69 MiB

Last scrub: never
```

### Default — degraded (1 missing device)

```
Pool:     /mnt/storage
Status:   DEGRADED (1 missing device)
Drives:   2 present, 1 missing
Profile:  RAID1

Capacity:
  Total:  768.00 MiB
  Used:   128.00 KiB
  Free:   255.69 MiB

Last scrub: never
```

### `--verbose` — adds per-disk section

```
Disks:
  virtio-disk1      devid 1   present
    Device:  /dev/disk/by-id/virtio-disk1
    Model:   QEMU HARDDISK
    Serial:  disk1
    LUKS:    xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    Errors:  read 0 / write 0 / corruption 0

  virtio-disk3      devid 3   MISSING
    Device:  /dev/disk/by-id/virtio-disk3  (not found)
    Errors:  unknown (device absent)
```

## Data sources

| Field | Command | Parse |
|-------|---------|-------|
| Drive count | `btrfs filesystem show $MOUNT` | Parse `devid` lines structurally — lines with a `/dev/mapper/` path are present, lines with `missing` in path position are missing. Derive counts from this structured parse. |
| Missing count | (same parse as above) | Don't grep-count "missing" separately — derive from per-device state |
| Profile | `btrfs filesystem df $MOUNT` | Grep for `Data,` line specifically (not first line — Metadata/System lines may precede it) |
| Capacity | `btrfs filesystem usage --raw $MOUNT` | Parse byte values from `Device size:`, `Used:`, `Free (estimated):` lines; format to human-readable ourselves |
| Last scrub | `btrfs scrub status $MOUNT \|\| true` | **Must capture with `\|\| true`** — exits non-zero when no scrub has run. Branch on "no stats available" vs real scrub data. |
| Per-disk mapper + devid | `btrfs filesystem show $MOUNT` | Parse `devid N ... /dev/mapper/X` lines |
| Per-disk errors | `btrfs device stats $MOUNT` | Parse `[/dev/mapper/X].write_io_errs 0` etc. |
| Model / Serial | `lsblk -ndo MODEL,SERIAL` on by-id path | Resolve by-id → real dev first |
| LUKS UUID | `cryptsetup luksUUID <by-id-path>` | Direct |
| By-id path | Reverse from mapper name | Mapper name = basename of by-id path (braid convention). Match against config.json disks. |
| Missing disk identity | Config diff | Configured disks minus present mappers = missing disks |

## Step-by-step implementation

### Step 1: Write test — `tests/8-braid-status.nix` + `tests/braid-status.py`

**`8-braid-status.nix`** — VM with 3 virtual disks, config.json for all 3, both `braid-add-disk` and `braid-status` in packages. Follow pattern of `5-braid-add-disk.nix`.

Comment header:
```
# Test: braid-status
#
# What: Runs braid-status in summary and verbose modes against a healthy
# 3-disk RAID1 pool, then simulates a drive failure and verifies degraded
# output. Also tests error on unmounted pool.
#
# Why: braid-status is the operator's primary diagnostic tool. It reads
# live btrfs/LUKS state, so it must be tested in a real VM with real
# filesystems to validate parsing of actual command output.
#
# Dependencies: braid-add-disk (pool creation).
```

**`braid-status.py`** — phases:

1. **Setup — single disk:** Use `braid-add-disk` to add disk1 only.

2. **Single-disk summary:** Run `braid-status`. Assert: `"healthy"`, `"1"` (drive count), `"single"` (profile), capacity fields present. Assert `"RAID1"` NOT present, `"missing"` NOT present.

3. **Setup — RAID1:** Use `braid-add-disk` to add disk2 and disk3 (converts to RAID1).

4. **Healthy RAID1 summary:** Run `braid-status`. Assert: `"healthy"`, `"3"` (drive count), `"RAID1"`, `"Total:"`, `"Used:"`, `"Free:"`, `"scrub"` present. Assert `"missing"` NOT present.

5. **Healthy verbose:** Run `braid-status --verbose`. Assert per-disk: each of `"virtio-disk1"`, `"virtio-disk2"`, `"virtio-disk3"` appears with `"present"` on same logical block. Assert `"devid"`, `"LUKS:"`, `"Errors:"` present. (Don't count "present" globally — assert per disk.)

6. **Simulate failure:** Unmount, close disk3 LUKS, remount with `-o degraded`.

7. **Degraded summary:** Assert: `"DEGRADED"`, `"missing"`, `"RAID1"`, drive counts (2 present, 1 missing).

8. **Degraded verbose:** Assert: `"MISSING"` present, `"virtio-disk3"` present, `"not found"` or `"device absent"` present. disk1 and disk2 still show as present.

9. **Error: unmounted pool:** Unmount pool, run `braid-status`, assert failure with `"not mounted"` in output.

### Step 2: Register test in `flake.nix`

Add line after the `replace-failed-disk` entry:
```nix
braid-status = pkgs.testers.nixosTest (import ./tests/8-braid-status.nix);
```

### Step 3: Run test, confirm it fails

`make test-one t=braid-status` — should fail because `braid-status` command doesn't exist.

### Step 4: Write script — `scripts/braid-status.sh`

Structure:

```
# Parse arguments (--verbose, --config)
# Read config (same pattern as braid-add-disk)
# Validate pool is mounted as btrfs
# Gather all btrfs data (5 commands)
# Parse pool-level fields
# Print summary
# If --verbose: parse per-disk fields, print disk detail
```

Key design decisions:
- **Exit 0** for successful status report (even if degraded). **Exit 1** only for actual errors (pool not mounted, config not found).
- **Mapper → by-id mapping** via config.json: for each mapper name in btrfs output, find the config disk whose basename matches.
- **Missing disk identification** by diffing config disks against present mappers.
- **Scrub handling:** `btrfs scrub status` exits non-zero when no scrub has run (`set -e` from `writeShellApplication` would kill the script). Must capture with `|| true` and branch on output content.
- **Profile parsing:** grep for `Data,` line in `btrfs filesystem df` — not first line (Metadata/System lines may come first).
- **Capacity parsing:** use `btrfs filesystem usage --raw` for byte values, format to human-readable ourselves. Avoids locale/version fragility.
- **Structured devid parsing:** parse `btrfs filesystem show` devid lines into a per-device table (devid, mapper path or "missing"). Derive present/missing counts and per-disk state from this single parse.
- **Unknown mapper** (not in config): print mapper name, note "not in config" — don't crash.

Runtime dependencies: `cryptsetup`, `btrfs-progs`, `util-linux`, `jq` (same as braid-add-disk).

### Step 5: Update `modules/braid/cli.nix`

Add `braid-status` package definition (same pattern as `braid-add-disk`) and add to `environment.systemPackages`.

### Step 6: Run test, confirm it passes

`make test-one t=braid-status`

### Step 7: Update docs

- **`README.md`**: Add braid-status usage examples (remove "not yet implemented" notice).
- **`AGENTS.md`**: Check off the two braid-status test plan items.

## Files to create

- `scripts/braid-status.sh` — the script
- `tests/8-braid-status.nix` — test VM config
- `tests/braid-status.py` — test assertions

## Files to modify

- `modules/braid/cli.nix` — add braid-status package + systemPackages entry
- `flake.nix` — register test
- `README.md` — document braid-status usage
- `AGENTS.md` — check off test plan items

## Verification

1. `make test-one t=braid-status` — full test suite passes
2. `make test` — all existing tests still pass (no regressions)
