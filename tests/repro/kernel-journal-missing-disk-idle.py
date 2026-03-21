# Repro: kernel journal on quiet missing-disk event
#
# Intent: Observe what the kernel journal records when one member of a mounted
# LUKS+btrfs RAID1 pool disappears and no subsequent filesystem I/O is forced.
#
# Why it exists: If braid stops using `btrfs filesystem show` for periodic
# missing-device detection, passive monitoring would need the kernel journal to
# surface disappearance events without requiring active probing.
#
# Scenario: A 3-disk RAID1 pool is mounted normally. QEMU sends an ACPI
# hot-unplug via `device_del` for one member, triggering the proper kernel
# device-removal path (same as physical SATA hot-unplug). No follow-up
# filesystem I/O is forced. The test then inspects only the post-marker kernel
# journal to see whether the disappearance itself emits useful structured
# kernel evidence.

import json


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
mount = "/mnt/storage"
marker = "BRAID_REPRO_MISSING_IDLE_START"
disks = ["disk1", "disk2", "disk3"]

victim = {
    "device_id": "disk2dev",
    "by_id": "/dev/disk/by-id/virtio-disk2",
    "mapper": "/dev/mapper/disk2",
    "label": "disk2",
}


def kernel_entries_after_marker():
    raw = machine.succeed("journalctl -o json --no-pager")
    entries = []
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            entries.append(json.loads(line))
        except json.JSONDecodeError:
            continue

    seen_marker = False
    out = []
    for entry in entries:
        msg = entry.get("MESSAGE", "")
        if msg == marker:
            seen_marker = True
            continue
        if seen_marker:
            out.append(entry)
    return out


def kernel_marker_present():
    raw = machine.succeed("journalctl -o json --no-pager")
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        if entry.get("MESSAGE", "") == marker:
            return True
    return False


def hot_unplug_device(device_id):
    machine.send_monitor_command(f"device_del {device_id}")


def print_interesting(entries):
    interesting = []
    for entry in entries:
        msg = entry.get("MESSAGE", "")
        if (
            "BTRFS" in msg
            or "I/O error" in msg
            or "Buffer I/O error" in msg
            or "blk_update_request" in msg
            or "device-mapper" in msg
            or "rejecting I/O" in msg
            or "detaching" in msg
            or "virtio" in msg
        ):
            interesting.append(entry)

    print("Interesting kernel journal entries after marker:")
    for entry in interesting:
        print(
            json.dumps(
                {
                    "MESSAGE": entry.get("MESSAGE"),
                    "_KERNEL_DEVICE": entry.get("_KERNEL_DEVICE"),
                    "_KERNEL_SUBSYSTEM": entry.get("_KERNEL_SUBSYSTEM"),
                    "_UDEV_SYSNAME": entry.get("_UDEV_SYSNAME"),
                    "_UDEV_DEVNODE": entry.get("_UDEV_DEVNODE"),
                    "_UDEV_DEVLINK": entry.get("_UDEV_DEVLINK"),
                },
                indent=2,
                sort_keys=True,
            )
        )
    return interesting


with subtest("Setup: 3-disk LUKS + btrfs RAID1 pool mounted normally"):
    for name in disks:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
            f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}"
        )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 "
        "/dev/mapper/disk1 /dev/mapper/disk2 /dev/mapper/disk3"
    )
    machine.succeed(f"mkdir -p {mount}")
    machine.succeed(f"mount /dev/mapper/disk1 {mount}")
    machine.succeed(f"echo 'healthy data' > {mount}/healthy.txt")
    machine.succeed("sync")

with subtest("Hot-unplug one disk while mounted, with no follow-up I/O"):
    machine.succeed(f"printf '<6>{marker}\\n' > /dev/kmsg")
    hot_unplug_device(victim["device_id"])
    machine.wait_until_fails(f"test -e {victim['by_id']}", timeout=10)
    machine.succeed("journalctl --sync")

with subtest("Pool remains mounted after device removal"):
    machine.succeed(f"mountpoint -q {mount}")

with subtest("Kernel journal after marker is captured for analysis"):
    assert kernel_marker_present(), "Expected repro marker to be present in kernel journal"
    entries = kernel_entries_after_marker()
    interesting = print_interesting(entries)
    print(f"Found {len(entries)} total kernel entries after quiet disappearance")
    print(f"Found {len(interesting)} interesting entries after quiet disappearance")
    # Virtio PCI hot-unplug with no follow-up I/O produces no journal entries.
    # The device IS removed (confirmed by wait_until_fails on by-id symlink above),
    # but the journal is silent. This is the expected finding for idle disappearance.

machine.shutdown()
