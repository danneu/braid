# Repro: kernel journal on missing-disk event followed by filesystem I/O
#
# Intent: Observe what the kernel journal records when one member of a mounted
# LUKS+btrfs RAID1 pool disappears and the filesystem later performs reads and
# writes through the degraded mount.
#
# Why it exists: Passive monitoring may miss a quiet disappearance until the
# filesystem actually touches the missing member. This repro determines whether
# post-disappearance I/O reliably produces journal evidence that braid could
# alert on without `btrfs filesystem show`.
#
# Scenario: A 3-disk RAID1 pool is mounted normally. QEMU sends an ACPI
# hot-unplug via `device_del` for one member, triggering the proper kernel
# device-removal path (same as physical SATA hot-unplug). The test then forces
# a read and a write+fsync on the mounted filesystem, and inspects only the
# post-marker kernel journal.

import json


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
mount = "/mnt/storage"
marker = "BRAID_REPRO_MISSING_IO_START"
victim_file = f"{mount}/victim.txt"
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


def interesting_entries(entries):
    out = []
    for entry in entries:
        msg = entry.get("MESSAGE", "")
        if (
            "BTRFS error" in msg
            or "I/O error" in msg
            or "Buffer I/O error" in msg
            or "blk_update_request" in msg
            or "critical medium error" in msg
            or "device-mapper" in msg
            or "rejecting I/O" in msg
            or "virtio" in msg
        ):
            out.append(entry)
    return out


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
    machine.succeed(f"echo 'victim data' > {victim_file}")
    machine.succeed("sync")

with subtest("Hot-unplug one disk while mounted"):
    machine.succeed(f"printf '<6>{marker}\\n' > /dev/kmsg")
    hot_unplug_device(victim["device_id"])
    machine.wait_until_fails(f"test -e {victim['by_id']}", timeout=10)
    machine.succeed("journalctl --sync")

with subtest("Pool remains mounted after device removal"):
    machine.succeed(f"mountpoint -q {mount}")

with subtest("Follow-up read and write exercise the degraded filesystem"):
    read_back = machine.succeed(f"cat {victim_file}").strip()
    assert read_back == "victim data", f"Expected victim data, got: {read_back}"
    machine.succeed(
        f"dd if=/dev/zero of={mount}/post-fail.bin bs=1M count=8 conv=fsync status=none"
    )
    machine.succeed("sync")
    machine.succeed("sleep 1")

with subtest("Kernel journal after marker is captured for analysis"):
    assert kernel_marker_present(), "Expected repro marker to be present in kernel journal"
    entries = kernel_entries_after_marker()
    interesting = interesting_entries(entries)

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
    print(f"Found {len(entries)} total kernel entries after disappearance + I/O")
    print(f"Found {len(interesting)} interesting entries after disappearance + I/O")
    assert len(interesting) > 0, "Expected kernel journal entries about device removal"

machine.shutdown()
