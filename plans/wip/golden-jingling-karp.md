# Plan: fix repro VM tests to use `device_del` for realistic hot-unplug

## Context

The 4 repro tests in `tests/repro/` use QEMU's `drive_del` to simulate disk removal. `drive_del` only removes the host-side backing file — the guest kernel never sees the device disappear, so no udev remove events or kernel journal detach messages fire. This is why these tests "don't work" (per the commit message).

Real-world testing (2026-03-21) confirmed that physical SATA hot-unplug produces instant `ata*: SATA link down` journal messages and udev `ACTION=remove` events. The fix is to use QEMU's `device_del` instead, which sends an ACPI hot-unplug to the guest and triggers the proper kernel device-removal path.

## Victim identity fix

The current tests are inconsistent about which disk they remove:

- kernel-journal tests call `delete_backing_drive("empty2.qcow2")` — that's `emptyDiskImages[2]` which is **disk3** (serial `disk3`, id `disk3dev`)
- udev tests set `victim_by_id = "/dev/disk/by-id/virtio-disk3"` and filter for `disk3` — consistent with the kernel-journal tests' actual target, but confusing since `empty2` looks like "disk2"

Standardize all 4 tests on **disk2** as the victim (the middle disk — not disk1 which is used for `mount /dev/mapper/disk1`). This means:

- `device_del disk2dev`
- victim by-id: `/dev/disk/by-id/virtio-disk2`
- LUKS mapper: `/dev/mapper/disk2`
- filter candidates reference `disk2` everywhere

Each test gets a single victim dict at the top:

```python
victim = {
    "device_id": "disk2dev",
    "by_id": "/dev/disk/by-id/virtio-disk2",
    "mapper": "/dev/mapper/disk2",
    "label": "disk2",
}
```

All unplug calls, waits, filters, and assertions derive from this one source of truth.

## Changes

### Files to modify

1. `tests/repro/kernel-journal-missing-disk-idle.py`
2. `tests/repro/kernel-journal-missing-disk-io.py`
3. `tests/repro/udev-missing-disk-idle.py`
4. `tests/repro/udev-missing-disk-io.py`

No `.nix` changes needed — the nix files already set `id = "disk2dev"` in `deviceExtraOpts`.

### Removal mechanism

Replace `qemu_drive_for_image()` + `delete_backing_drive()` with:

```python
def hot_unplug_device(device_id):
    machine.send_monitor_command(f"device_del {device_id}")
```

Call `hot_unplug_device(victim["device_id"])`. Remove the now-dead `qemu_drive_for_image()` helper.

### Post-unplug synchronization

Replace blind `sleep` calls with a wait on a guest-visible condition — the victim's by-id symlink disappearing:

```python
machine.wait_until_fails(f"test -e {victim['by_id']}", timeout=10)
```

This confirms the guest kernel has processed the ACPI unplug before collecting logs. Eliminates flakiness from fixed sleeps on slow builders.

### Assertions

Add explicit assertions so the tests actually fail if removal evidence is absent:

**kernel-journal tests:** assert at least one interesting entry was found:

```python
assert len(interesting) > 0, "Expected kernel journal entries about device removal"
```

**udev tests:** define a predicate derived from the victim dict and use it for both filtering and the assertion, so printed diagnostics and pass/fail cannot drift:

```python
def is_victim_remove(event):
    return (
        event.get("ACTION") == "remove"
        and victim["label"] in (event.get("DEVNAME", "") + event.get("DM_NAME", ""))
    )

remove_events = [e for e in parsed if is_victim_remove(e)]
assert len(remove_events) > 0, "Expected udev ACTION=remove event for victim disk"
```

### Update test docstrings

Update scenario descriptions to reflect that `device_del` triggers a proper guest-visible device removal via ACPI hot-unplug. Remove language about "guest-visible block device remains present."

## Verification

```
just test repro-kernel-journal-missing-disk-idle repro-kernel-journal-missing-disk-io repro-udev-missing-disk-idle repro-udev-missing-disk-io
```

Expected: tests capture actual kernel journal entries (virtio device removal) and udev `ACTION=remove` events for disk2, and fail if those signals are absent. Run without `-v` first; add `-v` to a specific test only if output is unclear.
