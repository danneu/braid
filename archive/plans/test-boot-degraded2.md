# Plan: `4-degraded-boot` test

## Context

When a drive dies in the btrfs RAID1 NAS, the system must not hang forever or require a NixOS rebuild to boot. This test verifies that **proactive configuration** — `nofail` on LUKS, `degraded` on btrfs mount, `wants` instead of `requires` on device-scan — lets the system boot and serve data with N-1 drives.

The test is based on `4-remote-unlock` (same 2-machine initrd SSH pattern) with four targeted changes.

## Research findings

**Confirmed working:**
- `crypttabExtraOpts` — hidden NixOS option, `listOf singleLineStr`, systemd initrd only. Appends to crypttab options column.
- `nofail` in crypttab — makes cryptsetup unit `Wants=` instead of `Requires=` of `cryptsetup.target`. Boot continues if unlock fails. (NixOS/nixpkgs#74281)
- `x-systemd.device-timeout` — prevents 90s default wait for missing device. (systemd crypttab(5) man page). Use 10s in tests (VMs have no spin-up delay), 30s in production (real drives may be slow to enumerate).
- `-o degraded` — harmless when all devices present, required when member is missing.

**Risk: udev `SYSTEMD_READY=0`** (systemd/systemd#36886)
When btrfs has a missing member, udev may mark remaining devices as not ready, blocking mount. May not apply in initrd (btrfs udev rules might not be present). TDD will reveal; fallback is a custom udev rule or moving mount into the scan service script.

## Files

| Action | File | Description |
|--------|------|-------------|
| Create | `tests/4-degraded-boot.nix` | NixOS config (based on `4-remote-unlock.nix`) |
| Create | `tests/degraded-boot.py` | Test script |
| Edit | `flake.nix` | Add `degraded-boot` to checks |

## Changes from `4-remote-unlock.nix` (4 targeted diffs)

### 1. LUKS devices: add `crypttabExtraOpts`

```nix
# Was:
lib.genAttrs disks (name: {
  device = "/dev/disk/by-id/virtio-${name}";
})

# Now:
lib.genAttrs disks (name: {
  device = "/dev/disk/by-id/virtio-${name}";
  crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ];
})
```

### 2. Initrd `btrfs-device-scan`: `wants` instead of `requires`

```nix
# Was:
requires = map (d: "systemd-cryptsetup@${d}.service") disks;

# Now:
wants = map (d: "systemd-cryptsetup@${d}.service") disks;
```

`Wants + After` = "run after they finish, succeed or fail." With `Requires`, disk3's failure would cascade and fail the scan.

### 3. Mount options: add `"degraded"`

```nix
options = [
  "degraded"  # NEW — allows btrfs to mount with missing members
  "x-systemd.requires=btrfs-device-scan.service"
  "x-systemd.after=btrfs-device-scan.service"
];
```

### 4. Fixture script: write test data, then brick disk3

After the existing RAID1 creation and before closing LUKS devices, add:

```bash
# Mount and write test data
mkdir -p /tmp/fixture-mount
mount /dev/mapper/disk1-fmt /tmp/fixture-mount
echo 'data written before drive death' > /tmp/fixture-mount/survived.txt
sync
umount /tmp/fixture-mount
```

After closing all LUKS devices, add:

```bash
# Brick disk3 — zero the LUKS header
dd if=/dev/zero of=/dev/disk/by-id/virtio-disk3 bs=1M count=10
```

When `systemd-cryptsetup@disk3` reads this, `crypt_load()` should fail (not a valid LUKS device). However, the unit may still enter "activating" (waiting for password) before checking the header — this is why the client must also restart disk3's unit to force it to a terminal state.

## Test script (`degraded-boot.py`)

Same SSH-unlock pattern as `remote-unlock.py`, with these differences:
- Client unlocks disk1 and disk2 via SSH (disk3 skipped — corrupted header)
- Client restarts **all 3** cryptsetup units. disk3's restart fails immediately (bad header), which transitions it to a terminal state. This is critical: without restarting disk3, its unit may stay "activating" (waiting for password), and `After=` on btrfs-device-scan would block forever.
- Subtests verify:
  1. Initrd SSH is up with ask-password requests
  2. Unlock disk1 and disk2 over SSH; restart all 3 cryptsetup units (disk3 fails, that's fine)
  3. Server reaches `multi-user.target` (doesn't hang)
  4. btrfs mounted — disk1+disk2 present, `"missing"` in `btrfs fi show` output
  5. Pre-existing data (`survived.txt`) is intact
  6. New writes work in degraded mode
  7. Journal contains evidence of disk3 failure (`journalctl -b -u systemd-cryptsetup@disk3` — the initrd systemd instance is gone after switch-root, so checking `systemctl is-failed` in stage-2 would return "not found")

## `flake.nix` change

Add one line after `remote-unlock`:

```nix
degraded-boot = pkgs.testers.nixosTest (import ./tests/4-degraded-boot.nix);
```

## Verification

```bash
make test-one t=degraded-boot
```

If it hangs at mount (udev `SYSTEMD_READY=0` issue), fallback: move the mount into the `btrfs-device-scan` script (post-unlock, so `/dev/mapper/disk1` is the real mapper name, not the `-fmt` fixture name):
```bash
btrfs device scan
mount -o degraded /dev/mapper/disk1 /mnt/storage 2>/dev/null || true
```

## What this proves for production

If the test passes, the production NixOS config needs only 3 additions to be degraded-boot-tolerant — no rebuild needed when a drive dies:
1. `crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=30s" ]` on each LUKS device
2. `wants` instead of `requires` in the btrfs-device-scan service
3. `"degraded"` in the btrfs mount options
