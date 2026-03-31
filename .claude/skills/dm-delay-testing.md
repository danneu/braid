---
name: dm-delay-testing
description: How to use Linux dm-delay to throttle disk I/O in NixOS VM tests when operations complete too fast to observe
---

# dm-delay: Throttling Disk I/O in VM Tests

NixOS VM tests have no real disk latency — operations that take seconds on real hardware complete in milliseconds. When a test needs to observe an in-progress state (e.g., polling `btrfs device usage` during a device remove), the operation can finish before the first poll fires.

## Solution: dm-delay

dm-delay is a device-mapper target that adds a configurable per-I/O delay. Stack it under LUKS on the target disk only:

```
/dev/disk/by-id/virtio-diskN
  → /dev/mapper/diskN-delay   (dm-delay)
    → /dev/mapper/diskN        (LUKS)
      → filesystem
```

## Setup pattern

Follows the same pattern as dm-flakey/dm-dust in `tests/repro/kernel-journal-write-error.py` and `tests/repro/kernel-journal-bad-sector.py`: all dm setup, reconfiguration, and cleanup happens in the Python test script.

### Nix config

Add `pkgs.lvm2` to `environment.systemPackages` (provides `dmsetup`). No kernel module config needed — use `modprobe` in the test script.

### Python test script

```python
DISK_RAW = "/dev/disk/by-id/virtio-diskN"
DISK_DM = "diskN-delay"

def dm_delay_table(delay_ms):
    sectors = machine.succeed(f"blockdev --getsz {DISK_RAW}").strip()
    return f"0 {sectors} delay {DISK_RAW} 0 {delay_ms}"

def dm_delay_create():
    machine.succeed("modprobe dm-delay")
    machine.succeed(f"dmsetup create {DISK_DM} --table '{dm_delay_table(0)}'")

def dm_delay_activate(delay_ms):
    """Live-swap table to inject delay. Safe while filesystem is mounted."""
    machine.succeed(f"dmsetup suspend {DISK_DM}")
    machine.succeed(f"dmsetup reload {DISK_DM} --table '{dm_delay_table(delay_ms)}'")
    machine.succeed(f"dmsetup resume {DISK_DM}")
```

### Workflow

1. Create dm-delay with 0ms delay (fast setup)
2. Open LUKS on `/dev/mapper/diskN-delay`
3. Set up filesystem, write data — all at full speed
4. Before the operation you need to observe: `dm_delay_activate(100)` to inject 100ms per-I/O delay
5. Run the operation + polling loop

### Choosing a delay value

100ms per I/O is a good starting point. Each btrfs block group relocation involves many I/Os, so 100ms stretches a 0.03s operation to ~10-20s — plenty of polling time.

## Live example

`tests/progress-monitoring.py` uses this pattern to observe `btrfs device remove` progress on disk3.

## Key details

- dm-delay delays every BIO (block I/O request), not filesystem-level operations
- `dmsetup suspend/reload/resume` is a standard live table swap — I/O queues briefly during suspend, then drains with the new delay
- Only throttle the disk you need to observe — leave other disks fast
- No cleanup needed if the VM is destroyed after the test
