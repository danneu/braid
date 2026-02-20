# Plan: Implement `braid-remove-disk`

## Context

`braid-add-disk` exists and is fully tested. The project now needs `braid-remove-disk` — the symmetric counterpart for gracefully removing disks from the pool. The full spec is settled in `docs/decisions/disk-pool-management.md`. The project uses TDD: write failing tests first, then implement.

## Files to Create

| File | Purpose |
|------|---------|
| `scripts/braid-remove-disk.sh` | The remove script |
| `tests/8-braid-remove-disk.nix` | Test harness (Nix) |
| `tests/braid-remove-disk.py` | Test logic (Python) |

## Files to Modify

| File | Change |
|------|--------|
| `modules/braid/cli.nix` | Add `braid-remove-disk` as writeShellApplication alongside `braid-add-disk` |
| `flake.nix` | Register `braid-remove-disk` test in `checksFor` |
| `tests/braid-module/01-single-disk.py` | Add subtest asserting `braid-remove-disk` is on PATH |
| `AGENTS.md` | Check off 4 test plan items |
| `README.md` | Remove "not yet implemented" caveat (lines 74-76, 114) |

## Step 1: Test File — `tests/8-braid-remove-disk.nix`

Single test file covering all four AGENTS.md test plan items as subtests (matches how `5-braid-add-disk.nix` covers add's full lifecycle in one file).

**Node setup:**
- 3 virtual disks (256MB each, serials disk1/disk2/disk3)
- Both `braid-add-disk` and `braid-remove-disk` packaged via `writeShellApplication` (same pattern as `5-braid-add-disk.nix:26-29`)
- Config.json starts with all 3 disks (so `braid-add-disk` can build the pool)

**Key test technique:** Use `--config /tmp/braid-config.json` to pass different config files to the script, simulating `nixos-rebuild switch` removing a disk from config. This avoids mutating `/etc` (which is Nix-managed) and is deterministic. The `/etc/braid/config.json` remains untouched with all 3 disks for `braid-add-disk` to use during pool setup.

Block comment convention (from existing tests):
```
# What: Runs braid-remove-disk through its lifecycle: graceful remove, remove-missing,
# LUKS cleanup, redundancy warning, and validation errors.
#
# Why: Symmetric counterpart to braid-add-disk. Must handle both happy path (disk
# present, data migrates off) and failure path (disk gone, remove missing).
#
# Dependencies: braid-add-disk (builds the test pool).
```

## Step 2: Test Logic — `tests/braid-remove-disk.py`

### Helpers

```python
def write_config(disk_list):
    """Write a config file to /tmp simulating nixos-rebuild switch."""
    import json
    config = json.dumps({"disks": disk_list, "mountPoint": "/mnt/storage"})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"

def remove_disk(dev, phrase="remove this disk"):
    return f"echo '{phrase}' | braid-remove-disk --config /tmp/braid-config.json {dev}"
```

Note: `remove_disk` always uses `--config /tmp/braid-config.json`. The test writes different configs to `/tmp` via `write_config()` before each phase, keeping `/etc/braid/config.json` untouched (it stays with all 3 disks for `braid-add-disk` pool setup).

### Subtests (in order)

**Phase 0 — Setup:** Build 3-drive RAID1 pool with `braid-add-disk`. Write test data.

**Phase 1 — Validation errors:**
- Non-by-id path rejected (`/dev/vdb`)
- Disk still in config rejected (symmetric guard: "still in braid.disks")

**Phase 2 — Graceful remove (Tier 1):**
- Write config without disk3 → run `braid-remove-disk /dev/disk/by-id/virtio-disk3`
- Assert: disk3 gone from pool, disk1+disk2 remain, RAID1 profile, data intact
- Assert: LUKS mapper `/dev/mapper/virtio-disk3` closed (LUKS cleanup)

**Phase 3 — No-args listing:**
- Run `braid-remove-disk` with no args, verify it shows "Removable disks" output

**Phase 4 — Redundancy warning:**
- Write config with only disk1 → try removing disk2
- Normal phrase "remove this disk" should fail/abort
- Escalated phrase "remove this disk without redundancy" succeeds
- Assert: disk2 gone, pool has 1 device, data intact

**Phase 5 — Rebuild pool:** Re-add disk2 and disk3 for Tier 2 test.

**Phase 6 — Remove-missing (Tier 2):**
- Simulate disk3 death: unmount pool → `cryptsetup luksClose virtio-disk3` → `mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage` (unmount first because `cryptsetup close` fails on a mapper in use by a mounted filesystem)
- Write config without disk3 → run `braid-remove-disk /dev/disk/by-id/virtio-disk3`
- Assert: no "missing" in pool, data intact

**Phase 7 — Tier 3 (fail with diagnostic):**
- Try to remove a disk that's neither open nor missing → clear error message

## Step 3: Script — `scripts/braid-remove-disk.sh`

Linear procedural script mirroring `braid-add-disk.sh` structure. Same patterns: `--config` override, `jq` for config reading, `mapper_name=$(basename "$disk")`, exact-phrase confirmation.

### Section-by-section

**1. Read config** — Identical to braid-add-disk (lines 5-18): `CONFIG_FILE`, `--config` override, `MOUNT_POINT` from jq.

**2. No-args listing** — Show "Removable disks" (in pool but NOT in config). Get pool members from `btrfs fi show`, cross-reference with config. Also note if pool has a missing device.

**3. Validate arguments** — By-id path enforcement. Do NOT hard-fail on `! -b "$disk"` (device may be physically gone in Tier 2).

**4. Inverse config guard** — `jq -e` check that disk IS in `.disks` → error with guidance showing what to remove and `nixos-rebuild switch` instructions. Exact inverse of add's guard (add: `! index` = error; remove: `index` = error).

**5. Pool must be mounted** — `mountpoint -q "$MOUNT_POINT"` check. Unlike add (which can create a new pool), remove always needs an existing pool.

**6. Three-tier detection:**
- **Tier 1 (graceful):** `mapper_path` exists → verify via `cryptsetup status` that it maps to the requested by-id disk → verify it's in the btrfs pool
- **Tier 2 (missing):** mapper doesn't exist AND `btrfs fi show` reports "missing" AND exactly 1 missing device. If multiple devices are missing, hard-fail with diagnostic: "Multiple missing devices detected. Cannot determine which to remove. Resolve manually with `btrfs device remove missing <mountpoint>`."
- **Tier 3 (fail):** neither condition met → diagnostic error

**7. Count remaining disks** — `grep -c "devid"` on `btrfs fi show` output, compute `remaining_after = count - 1`.

**8. Disk info + confirmation:**
- Show target info (model/size/serial for graceful, "not present" for missing)
- Show pool device count transition
- If `remaining_after < 2`: WARNING + "Type 'remove this disk without redundancy'"
- Otherwise: "Type 'remove this disk'"

**9. Execute remove:**
- Tier 1: `btrfs device remove /dev/mapper/$mapper_name $MOUNT_POINT`
- Tier 2: `btrfs device remove missing $MOUNT_POINT`

**10. LUKS cleanup (Tier 1 only):**
- `cryptsetup close $mapper_name`
- Success → "disk fully released — safe to physically remove"
- Failure (busy) → print actionable guidance (`fuser -vm`, `cryptsetup close`, "will close on reboot"), `exit 1`. This is a real failure, not best-effort — the spec requires non-zero exit so the user knows the disk is still held open.

**11. Summary** — Device count, profile, warning if single disk remains.

### Differences from braid-add-disk

- No passphrase needed (disk already open or gone)
- No `BRAID_LUKS_OPTS` or `BRAID_PASSPHRASE` env vars
- No ERR trap needed (btrfs remove is atomic per-device; nothing to roll back)
- Inverse config guard (still-in-config = error, not-in-config = proceed)
- Two confirmation phrases (normal vs redundancy warning)

## Step 4: Module Change — `modules/braid/cli.nix`

Add `braid-remove-disk` as a second `writeShellApplication` with the same `runtimeInputs` (`cryptsetup`, `btrfs-progs`, `util-linux`, `jq`), add to `environment.systemPackages`.

## Step 5: Register Test — `flake.nix`

Add after line 34 (replace-failed-disk):
```nix
braid-remove-disk = pkgs.testers.nixosTest (import ./tests/8-braid-remove-disk.nix);
```

## Step 6: Doc Updates

- `AGENTS.md`: Check off the 4 `braid-remove-disk` test plan items
- `README.md`: Remove TODO comment (line 74) and "Not yet implemented" paragraph (lines 74-76), remove HTML comment (line 114)

## Implementation Order

1. `tests/8-braid-remove-disk.nix` + `tests/braid-remove-disk.py` + `flake.nix` registration
2. Run test → confirm it fails (script doesn't exist)
3. `scripts/braid-remove-disk.sh`
4. Run test → confirm it passes
5. `modules/braid/cli.nix` + `tests/braid-module/01-single-disk.py` (add PATH assertion)
6. Doc updates (`AGENTS.md`, `README.md`)

## Step 7: Module Test — `tests/braid-module/01-single-disk.py`

Add a subtest to the existing module test verifying both CLI tools are on PATH when `braid.enable = true`:

```python
with subtest("CLI tools are on PATH"):
    machine.succeed("which braid-add-disk")
    machine.succeed("which braid-remove-disk")
```

This catches `cli.nix` packaging regressions. Added to `01-single-disk.py` because it already imports the braid module and boots a working system.

## Spec Gaps Identified

1. **Multiple missing devices:** `btrfs device remove missing` removes one arbitrary missing device. If multiple devices are missing, the script cannot guarantee which one gets evicted. Resolved: hard-fail when missing count > 1, with diagnostic guiding the user to `btrfs device remove missing` directly.

2. **Profile after 2→1 removal:** `btrfs device remove` on a 2-disk RAID1 needs to convert data to single profile before removing. The exact `btrfs fi df` output after this is TBD — the test will verify device count = 1 rather than asserting a specific profile string.

## Verification

```bash
make test-one t=braid-remove-disk        # New test passes
make test                                  # All existing tests still pass
```
