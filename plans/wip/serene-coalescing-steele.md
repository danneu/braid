# Fix progress-monitoring test: dm-delay throttle on disk3

## Context

The "device remove progress observed" subtest in `tests/progress-monitoring.py` fails because `btrfs device remove` completes in ~0.03s in the VM — too fast for the polling loop to observe an intermediate state where disk3's allocation bytes have decreased but disk3 is still listed. The VM has no real disk latency, so block group relocation is nearly instantaneous.

We'll use Linux device-mapper's `dm-delay` target to inject per-I/O latency on disk3 only, stretching the operation from milliseconds to seconds.

## Design

Stack for disk3 only:
```
/dev/disk/by-id/virtio-disk3
  → /dev/mapper/disk3-delay   (dm-delay: 0ms during setup, 50ms during remove)
    → /dev/mapper/disk3        (LUKS)
      → btrfs
```

disk1 and disk2 are unchanged — no delay layer, so setup/balance stays fast.

## Files to modify

### 1. `tests/progress-monitoring.nix`

Add `pkgs.lvm2` to `environment.systemPackages` (provides `dmsetup`). No kernel module config — `modprobe` happens in the test script.

### 2. `tests/progress-monitoring.py`

All dm-delay setup, reconfiguration, and cleanup happens in the Python test script (matching the pattern in `tests/repro/kernel-journal-write-error.py` and `tests/repro/kernel-journal-bad-sector.py`).

**Helper functions** (add near top, after constants):

```python
DISK3_RAW = "/dev/disk/by-id/virtio-disk3"
DISK3_DM = "disk3-delay"

def dm_delay_table(delay_ms):
    """dm-delay table for disk3 with given per-I/O delay."""
    sectors = machine.succeed(f"blockdev --getsz {DISK3_RAW}").strip()
    return f"0 {sectors} delay {DISK3_RAW} 0 {delay_ms}"

def dm_delay_create():
    """Create dm-delay wrapper on disk3 with zero delay."""
    machine.succeed("modprobe dm-delay")
    machine.succeed(f"dmsetup create {DISK3_DM} --table '{dm_delay_table(0)}'")

def dm_delay_activate(delay_ms):
    """Live-swap dm-delay table to inject real I/O delay."""
    machine.succeed(f"dmsetup suspend {DISK3_DM}")
    machine.succeed(f"dmsetup reload {DISK3_DM} --table '{dm_delay_table(delay_ms)}'")
    machine.succeed(f"dmsetup resume {DISK3_DM}")
```

**LUKS setup (lines 21-29):** Replace the uniform loop with disk3-specific handling:

- disk1, disk2: unchanged (LUKS directly on raw device)
- disk3: create dm-delay wrapper first, then LUKS on `/dev/mapper/disk3-delay`

```python
# LUKS format + open disk1 and disk2 directly
for name in ["disk1", "disk2"]:
    dev = f"/dev/disk/by-id/virtio-{name}"
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}"
    )
    machine.succeed(
        f"echo -n '{PASSPHRASE}' | cryptsetup luksOpen --key-file=- {dev} {name}"
    )

# disk3: dm-delay wrapper (0ms initially) → LUKS on top
dm_delay_create()
machine.succeed(
    f"echo -n '{PASSPHRASE}' | cryptsetup luksFormat --batch-mode --key-file=- "
    f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/mapper/{DISK3_DM}"
)
machine.succeed(
    f"echo -n '{PASSPHRASE}' | cryptsetup luksOpen --key-file=- /dev/mapper/{DISK3_DM} disk3"
)
```

**Before device-remove polling (before line 108):** Inject 50ms delay:

```python
dm_delay_activate(50)
```

**Cleanup** (add after fixture copy, at end of file):

```python
machine.execute("cryptsetup close disk3")
machine.execute(f"dmsetup remove {DISK3_DM}")
```

The existing polling loop + device-remove shell command (lines 108-119) stays unchanged.

## Why 50ms

Each block group relocation involves many I/O operations (reads + writes). At 50ms per I/O, even 3 block groups will take multiple seconds to relocate, giving the 0.05s-interval polling loop many opportunities to observe the intermediate state.

## Verification

Run `just test progress-monitoring` — the "device remove progress observed" subtest should pass and the fixture file `btrfs-device-usage-removing.txt` should be captured.
