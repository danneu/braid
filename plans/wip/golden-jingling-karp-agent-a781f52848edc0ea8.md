# Research: QEMU Disk Hot-Unplug in NixOS VM Tests

## Executive Summary

Your existing repro tests use `drive_del`, which only removes the host-side backing file. **The guest kernel never sees the device disappear** -- no udev remove events, no kernel journal "device removed" messages. This is why those tests don't trigger realistic disk removal signals.

To get proper guest-visible disk removal with udev events and kernel journal entries, you need `device_del` instead, which triggers a PCI/SCSI hot-remove that the guest kernel cooperates with via ACPI.

---

## 1. `drive_del` vs `device_del`

QEMU separates storage into two layers:

- **Drive** (host-side): the backing image file, caching policy, I/O mode
- **Device** (guest-side): the PCI/SCSI device visible to the guest kernel

| Command      | What it does                         | Guest sees removal?                                    | Udev events?                   | Kernel journal?                                               |
| ------------ | ------------------------------------ | ------------------------------------------------------ | ------------------------------ | ------------------------------------------------------------- |
| `drive_del`  | Removes host-side backing only       | No -- device stays in guest, I/O errors on next access | No remove event                | Only I/O errors when filesystem tries to use the dead backing |
| `device_del` | Removes the guest-visible PCI device | Yes -- proper hot-remove                               | Yes -- udev remove events fire | Yes -- kernel logs device removal                             |

`drive_del` is documented as a fallback for when `device_del` doesn't complete successfully. It should not be the primary removal mechanism.

**This explains why your existing repro tests produce no udev events and no kernel journal entries on idle -- the guest doesn't know anything happened until I/O hits the missing backing.**

## 2. How `device_del` works

`device_del <device-id>` sends an ACPI hot-unplug request to the guest:

1. QEMU sends a System Control Interrupt (SCI) to the guest
2. Guest kernel's ACPI handler processes the removal
3. Guest kernel brings the block device offline
4. Kernel emits remove events (udev, journal)
5. Guest tells QEMU the unplug is complete
6. QEMU destroys both the device and its associated drive object

**Requires guest cooperation** -- the guest kernel must have PCI hotplug support (`acpiphp` module). NixOS kernels have this built-in.

## 3. How NixOS `emptyDiskImages` translates to QEMU args

### Drive generation (from `qemu-vm.nix`)

The `driveCmdline` function (line 72) generates for each drive at index `idx`:

```
-drive index=<idx>,id=drive<idx>,if=none,file=<path>,werror=report,...
-device virtio-blk-pci,drive=drive<idx>,serial=<serial>,id=<id>,...
```

Key: `imap1` is used, so indices start at 1. The root drive is always drive1.

### For your existing test config:

```nix
virtualisation.emptyDiskImages = [
  { size = 512; driveConfig.deviceExtraOpts = { serial = "disk1"; id = "disk1dev"; }; }
  { size = 512; driveConfig.deviceExtraOpts = { serial = "disk2"; id = "disk2dev"; }; }
  { size = 512; driveConfig.deviceExtraOpts = { serial = "disk3"; id = "disk3dev"; }; }
];
```

This produces (root is drive1, then empty disks follow):

```
# Root drive
-drive index=1,id=drive1,if=none,file="$NIX_DISK_IMAGE",cache=writeback,werror=report
-device virtio-blk-pci,drive=drive1,bootindex=1,serial=root

# Empty disk 0 (index 2 in the merged drives list)
-drive index=2,id=drive2,if=none,file=$(pwd)/empty0.qcow2,werror=report
-device virtio-blk-pci,drive=drive2,serial=disk1,id=disk1dev

# Empty disk 1 (index 3)
-drive index=3,id=drive3,if=none,file=$(pwd)/empty1.qcow2,werror=report
-device virtio-blk-pci,drive=drive3,serial=disk2,id=disk2dev

# Empty disk 2 (index 4)
-drive index=4,id=drive4,if=none,file=$(pwd)/empty2.qcow2,werror=report
-device virtio-blk-pci,drive=drive4,serial=disk3,id=disk3dev
```

### Default device type

Default `diskInterface` is `"virtio"`, producing `virtio-blk-pci` devices. Guest sees them as `/dev/vda`, `/dev/vdb`, etc.

## 4. Using `device_del` with your existing config

Since you already set `id = "disk3dev"` in `deviceExtraOpts`, you can `device_del` by that ID:

```python
# Instead of:
machine.send_monitor_command(f"drive_del {drive}")

# Use:
machine.send_monitor_command("device_del disk3dev")
```

This will:

- Trigger ACPI hot-unplug in the guest
- Guest kernel removes `/dev/vdd` (or whichever vd\* it was)
- udev fires `ACTION=remove` events for the block device
- Kernel journal logs the PCI device removal
- LUKS layer on top sees the underlying device vanish
- btrfs sees the dm-crypt device disappear

**No changes to the Nix config needed** -- the `id` field in `deviceExtraOpts` is already the device ID that `device_del` uses.

## 5. SCSI / AHCI alternative for more realistic SATA messages

### Option A: `diskInterface = "scsi"` (globally)

```nix
virtualisation.qemu.diskInterface = "scsi";
```

Changes ALL drives (including root) to use:

```
-device lsi53c895a -device scsi-hd,drive=drive<N>,serial=<serial>,id=<id>
```

Guest sees `/dev/sda`, `/dev/sdb`, etc. Adds `sym53c8xx` kernel module.

**Problem**: This uses the LSI53C895A SCSI controller, not AHCI/SATA. So kernel messages will be SCSI-flavored, not SATA-flavored.

### Option B: Per-disk SCSI via `qemu.options` (manual)

Skip `emptyDiskImages` entirely and use `virtualisation.qemu.options` to pass raw QEMU args with virtio-scsi or AHCI:

```nix
virtualisation.qemu.options = [
  # Add virtio-scsi controller
  "-device virtio-scsi-pci,id=scsi0"
  # Add a SCSI disk
  "-drive file=$(pwd)/empty0.qcow2,if=none,id=drive_disk1,format=qcow2,werror=report"
  "-device scsi-hd,drive=drive_disk1,bus=scsi0.0,serial=disk1,id=disk1dev"
];
```

Then `device_del disk1dev` removes the `scsi-hd` device. Guest sees SCSI detach messages in dmesg.

### Option C: AHCI/SATA for most realistic messages

```nix
virtualisation.qemu.options = [
  "-device ahci,id=ahci0"
  "-drive file=$(pwd)/empty0.qcow2,if=none,id=drive_disk1,format=qcow2"
  "-device ide-hd,drive=drive_disk1,bus=ahci0.0,id=disk1dev"
];
```

Guest sees `/dev/sda` with AHCI/SATA stack. **However**: AHCI hot-unplug support in QEMU is limited. `device_del` on `ide-hd` may not trigger proper ACPI hot-remove. SATA hot-swap is less well-supported than virtio or SCSI in QEMU.

### Recommendation

**Use virtio-blk-pci with `device_del`** (your current setup + one line change). This gives you:

- Proper guest-visible device removal
- udev remove events
- Kernel journal entries
- Works reliably with NixOS VM test infrastructure

The kernel messages won't say "SATA" but they will be realistic device-removal messages. For braid's purposes (detecting that a disk disappeared), the detection mechanism (udev events, btrfs errors, kernel journal) works the same regardless of bus type.

If you specifically want SCSI-style messages, use `virtualisation.qemu.diskInterface = "scsi"` which is a one-line change and well-supported.

## 6. Concrete code change for existing repro tests

Replace the `drive_del` approach:

```python
# OLD (host-only, guest doesn't see removal):
def qemu_drive_for_image(image_name):
    out = machine.send_monitor_command("info block")
    for line in out.splitlines():
        if line.startswith("drive") and image_name in line:
            return line.split(":", 1)[0]
    raise AssertionError(...)

def delete_backing_drive(image_name):
    drive = qemu_drive_for_image(image_name)
    machine.send_monitor_command(f"drive_del {drive}")


# NEW (guest sees proper device removal):
def hot_unplug_disk(device_id):
    """Remove a disk device so the guest kernel sees a proper hot-unplug."""
    machine.send_monitor_command(f"device_del {device_id}")

# Usage:
hot_unplug_disk("disk3dev")  # matches id= in deviceExtraOpts
```

## 7. QMP alternative (structured API)

The NixOS test driver also exposes a QMP (QEMU Machine Protocol) client. You could use it instead of the HMP monitor:

```python
# QMP-based device_del (returns structured JSON):
machine.qmp_client.send("device_del", {"id": "disk3dev"})

# Wait for the DEVICE_DELETED event:
evt = machine.wait_for_qmp_event(
    lambda e: e.get("event") == "DEVICE_DELETED" and e.get("data", {}).get("device") == "disk3dev"
)
```

This is more robust than parsing HMP text output.

## 8. Summary of what to change

| Current state            | Problem                                           | Fix                                                                    |
| ------------------------ | ------------------------------------------------- | ---------------------------------------------------------------------- |
| Tests use `drive_del`    | Guest never sees removal, no udev/journal signals | Switch to `device_del <device-id>`                                     |
| No device ID set         | Can't reference device for `device_del`           | Already fixed -- your tests set `id = "disk3dev"` in `deviceExtraOpts` |
| virtio-blk-pci default   | Fine for detection testing                        | No change needed (or set `diskInterface = "scsi"` for SCSI messages)   |
| `emptyDiskImages` config | Already correct                                   | No change needed                                                       |
