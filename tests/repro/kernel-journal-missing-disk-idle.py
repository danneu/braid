# Repro: kernel journal on quiet missing-disk event
#
# Intent: Observe what the kernel journal records when one member of a mounted
# LUKS+btrfs RAID1 pool disappears and no subsequent filesystem I/O is forced.
#
# Why it exists: If braid stops using `btrfs filesystem show` for periodic
# missing-device detection, passive monitoring would need the kernel journal to
# surface disappearance events without requiring active probing.
#
# Scenario: A 3-disk RAID1 pool is mounted normally. QEMU deletes the host
# backing drive for one member while the guest-visible block device remains
# present. No follow-up filesystem I/O is forced. The test then inspects only
# the post-marker kernel journal to see whether the disappearance itself emits
# useful structured kernel evidence.

import json


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
mount = "/mnt/storage"
marker = "BRAID_REPRO_MISSING_IDLE_START"
disks = ["disk1", "disk2", "disk3"]


def kernel_entries_after_marker():
    raw = machine.succeed("journalctl -k -o json --no-pager")
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
    raw = machine.succeed("journalctl -k -o json --no-pager")
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


def qemu_drive_for_image(image_name):
    out = machine.send_monitor_command("info block")
    print(f"QEMU info block:\n{out}")
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("drive") and image_name in line:
            return line.split(":", 1)[0]
    raise AssertionError(f"Could not find QEMU drive for image {image_name!r} in:\n{out}")


def delete_backing_drive(image_name):
    drive = qemu_drive_for_image(image_name)
    machine.send_monitor_command(f"drive_del {drive}")


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

with subtest("Delete one backing drive while mounted, with no follow-up I/O"):
    machine.succeed(f"printf '<6>{marker}\\n' > /dev/kmsg")
    delete_backing_drive("empty2.qcow2")
    machine.succeed("sleep 1")

with subtest("Pool remains mounted after backing drive deletion"):
    machine.succeed(f"mountpoint -q {mount}")

with subtest("Kernel journal after marker is captured for analysis"):
    assert kernel_marker_present(), "Expected repro marker to be present in kernel journal"
    entries = kernel_entries_after_marker()
    interesting = print_interesting(entries)
    print(f"Found {len(entries)} total kernel entries after quiet disappearance")
    print(f"Found {len(interesting)} interesting entries after quiet disappearance")

machine.shutdown()
