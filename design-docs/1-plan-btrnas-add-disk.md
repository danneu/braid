# Plan: `btrnas-add-disk` script + tests

## Context

The repo has 10 passing VM tests proving every primitive works (LUKS, btrfs RAID1, grow, shrink, heal, degrade, remote-unlock, samba). No NixOS module or scripts exist yet. `btrnas-add-disk` is the first real deliverable — the one imperative command that orchestrates LUKS + btrfs for new disks. See `design-docs/1-btrnas-add-disk.md` for the full design.

## Files to create

| File | Purpose |
|------|---------|
| `scripts/btrnas-add-disk.sh` | The bash script |
| `nix/btrnas-add-disk.nix` | `writeShellApplication` package |
| `tests/5-btrnas-add-disk.nix` | NixOS VM test definition |
| `tests/btrnas-add-disk.py` | Test script |

## File to modify

| File | Change |
|------|--------|
| `flake.nix` | Add `btrnas-add-disk` to `checks` |

---

## Script flow (`scripts/btrnas-add-disk.sh`)

### Step 1: Validate arguments

- Exactly 1 arg, else print usage and exit 1
- `test -b "$disk"` — must be a block device

### Step 2: Check disk state (safety guards)

If `cryptsetup isLuks "$disk"`:
- Try to open it with the provided passphrase using a temp mapper name
- Check if the opened device contains a btrfs filesystem (`blkid`)
- **LUKS + btrfs that's in our pool** → "already in pool" error
- **LUKS + btrfs NOT in our pool** → error, tell user to wipe manually
- **LUKS + no filesystem** (crash recovery case) → warn, ask to re-use or abort
- Close the temp mapper

If NOT already LUKS, continue to formatting.

### Step 3: Detect pool state (protect against unmounted pool)

Before assuming "first disk", guard against an existing-but-unmounted pool:
- If `/mnt/storage` is mounted as btrfs → pool exists, use "add to pool" path
- If `/mnt/storage` is NOT mounted:
  - Run `blkid -t TYPE=crypto_LUKS` to check if any other LUKS devices exist on the system
  - If LUKS devices found on other drives → refuse, tell user to unlock existing pool first
  - If no other LUKS devices → safe to assume first disk

### Step 4: Print disk info + confirmation

- `lsblk -ndo MODEL,SIZE,SERIAL` (handle empty model from virtio with "unknown" fallback)
- Scary warning with different message depending on pool state
- `read -r confirmation` must equal `"erase this disk"`

### Step 5: Get passphrase

- If `BTRNAS_PASSPHRASE` env var set → use it (tests/scripting)
- Else if pool exists → prompt once, verify against existing LUKS device via `cryptsetup luksOpen --test-passphrase`
- Else (first disk) → prompt twice, confirm match

### Step 6: LUKS format + open

- Mapper name: `btrnas-$(basename "$disk")`
- Cleanup trap: `cryptsetup luksClose` on failure after `luksOpen`
- `BTRNAS_LUKS_OPTS` injected via bash array (shellcheck-clean)

### Step 7: btrfs

- Pool exists → `btrfs device add` + `btrfs balance start -dconvert=raid1 -mconvert=raid1`
- No pool → `mkfs.btrfs -f`, `mkdir -p /mnt/storage`, `mount`

### Step 8: Print next steps

- by-id path to add to `btrnas.disks`
- LUKS UUID
- `sudo nixos-rebuild switch`

---

## Environment variables (for tests/scripting)

- `BTRNAS_PASSPHRASE` — skips interactive passphrase prompt
- `BTRNAS_LUKS_OPTS` — extra `cryptsetup luksFormat` flags (tests: `--pbkdf pbkdf2 --pbkdf-force-iterations 1000`)

## Error handling

- `set -euo pipefail` (automatic from `writeShellApplication`)
- Cleanup trap: `cryptsetup luksClose` on failure after `luksOpen`
- `shellcheck` at build time (from `writeShellApplication`)
- `BTRNAS_LUKS_OPTS` handled via bash array to satisfy shellcheck SC2086

---

## Package (`nix/btrnas-add-disk.nix`)

```nix
{ pkgs }:
pkgs.writeShellApplication {
  name = "btrnas-add-disk";
  runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux ];
  text = builtins.readFile ../scripts/btrnas-add-disk.sh;
}
```

---

## Test design (`tests/5-btrnas-add-disk.nix` + `btrnas-add-disk.py`)

One test file with sequential phases (matches `btrfs-grow1` pattern).

**Test invocation pattern:**
```python
def add_disk(dev):
    return (
        f"echo 'erase this disk' | "
        f"BTRNAS_PASSPHRASE='{passphrase}' "
        f"BTRNAS_LUKS_OPTS='{luks_opts}' "
        f"btrnas-add-disk {dev}"
    )
```

### Phase 1 — First disk (no pool)

- `btrnas-add-disk /dev/disk/by-id/virtio-disk1`
- Assert: `/mnt/storage` mounted, `Data, single` profile, write a file

### Phase 2 — Second disk (convert to RAID1)

- `btrnas-add-disk /dev/disk/by-id/virtio-disk2`
- Assert: `Data, RAID1`, disk1 data intact, write another file

### Phase 3 — Third disk (add to RAID1)

- `btrnas-add-disk /dev/disk/by-id/virtio-disk3`
- Assert: all 3 mapper devices in pool (`btrnas-virtio-diskN`), all data intact

### Phase 4 — Validation errors

- No args → `machine.fail("btrnas-add-disk")`
- Non-existent device → `machine.fail("btrnas-add-disk /dev/nonexistent")`
- Disk already in pool → `machine.fail(add_disk("/dev/disk/by-id/virtio-disk1"))`, check "already" in output

### Phase 5 — Crash recovery

- Format a 4th disk as LUKS manually (no btrfs inside), simulating a crash between luksFormat and mkfs
- Run `btrnas-add-disk` on it → should detect the recoverable state (not just refuse)

Note: Need a 4th virtual disk for this test. Add to `virtualisation.emptyDiskImages`.

### Phase 6 — Unmounted pool guard

- Unmount `/mnt/storage`, close all LUKS devices (simulates post-reboot, pre-unlock)
- Run `btrnas-add-disk` on a new disk → should refuse because other LUKS devices exist
- Re-open LUKS devices, remount, verify pool is intact

---

## `flake.nix` change

Add one line to `checks.aarch64-darwin`:
```nix
btrnas-add-disk = pkgs.testers.nixosTest (import ./tests/5-btrnas-add-disk.nix);
```

---

## Verification

```
make test-one t=btrnas-add-disk
```

All existing tests remain unaffected (no files modified except `flake.nix` which only adds a new check).

---

## Out of scope (future work)

- **Drive replacement** (`btrnas-replace-disk`) — separate command for degraded pool recovery (`btrfs device remove missing` + add new)
- **/mnt/storage ownership** — module's job, not the script's. Module handles `chown` for Samba.
