# Repro: udev events on missing-disk event followed by filesystem I/O
#
# Intent: Observe what raw block-device udev monitoring reports when one member
# of a mounted LUKS+btrfs RAID1 pool disappears and the filesystem later
# performs reads and writes through the degraded mount.
#
# Why it exists: Passive disappearance detection may produce no useful signal
# until later filesystem activity touches the missing member. This repro checks
# whether follow-up I/O changes what udev reports.
#
# Scenario: A 3-disk RAID1 pool is mounted normally. QEMU sends an ACPI
# hot-unplug via `device_del` for one member, triggering the proper kernel
# device-removal path (same as physical SATA hot-unplug). The test forces a
# read, a write with fsync, and a sync, then asserts that at least one udev
# ACTION=remove event fires for the victim disk.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
mount = "/mnt/storage"
log_path = "/tmp/udev-monitor.log"
victim_file = f"{mount}/victim.txt"
disks = ["disk1", "disk2", "disk3"]

victim = {
    "device_id": "disk2dev",
    "by_id": "/dev/disk/by-id/virtio-disk2",
    "mapper": "/dev/mapper/disk2",
    "label": "disk2",
}


def hot_unplug_device(device_id):
    machine.send_monitor_command(f"device_del {device_id}")


def start_udev_monitor():
    machine.succeed(f"rm -f {log_path}")
    pid = machine.succeed(
        "sh -lc "
        f"\"stdbuf -oL -eL udevadm monitor --kernel --udev --subsystem-match=block --property > {log_path} 2>&1 & echo \\$!\""
    ).strip()
    assert pid.isdigit(), f"Expected numeric monitor PID, got: {pid!r}"
    machine.succeed(f"kill -0 {pid}")
    machine.succeed("sleep 1")
    return pid


def stop_udev_monitor(pid):
    machine.execute(f"kill {pid}")
    machine.execute(f"wait {pid} 2>/dev/null")
    machine.succeed("sleep 1")


def read_monitor_output():
    machine.succeed(f"test -r {log_path}")
    return machine.succeed(f"cat {log_path}")


def split_event_blocks(raw):
    blocks = []
    current = []
    for line in raw.splitlines():
        if line.strip():
            current.append(line)
            continue
        if current:
            blocks.append("\n".join(current))
            current = []
    if current:
        blocks.append("\n".join(current))
    return blocks


def parse_event_block(block):
    data = {}
    for line in block.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        data[key] = value
    return data


def print_event_blocks(title, blocks):
    print(f"{title}:")
    if not blocks:
        print("(none)")
        return
    for idx, block in enumerate(blocks, start=1):
        print(f"--- event {idx} ---")
        print(block)


def relevant_blocks(blocks, candidates):
    out = []
    lowered = [candidate.lower() for candidate in candidates if candidate]
    for block in blocks:
        haystack = block.lower()
        if any(candidate in haystack for candidate in lowered):
            out.append(block)
    return out


def is_victim_remove(event):
    return (
        event.get("ACTION") == "remove"
        and victim["label"]
        in (
            event.get("DEVNAME", "")
            + event.get("DM_NAME", "")
            + event.get("ID_SERIAL", "")
            + event.get("DEVLINKS", "")
        )
    )


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

with subtest("Start background udev monitor before hot-unplug"):
    victim_devname = machine.succeed(f"readlink -f {victim['by_id']}").strip()
    print(f"Victim by-id path: {victim['by_id']}")
    print(f"Victim raw device path before disappearance: {victim_devname}")
    monitor_pid = start_udev_monitor()
    print(f"udevadm monitor pid: {monitor_pid}")

with subtest("Hot-unplug one disk while mounted"):
    hot_unplug_device(victim["device_id"])
    machine.wait_until_fails(f"test -e {victim['by_id']}", timeout=10)

with subtest("Pool remains mounted after device removal"):
    machine.succeed(f"mountpoint -q {mount}")

with subtest("Follow-up read and write exercise the degraded filesystem"):
    read_back = machine.succeed(f"cat {victim_file}").strip()
    assert read_back == "victim data", f"Expected victim data, got: {read_back}"
    machine.succeed(
        f"dd if=/dev/zero of={mount}/post-fail.bin bs=1M count=8 conv=fsync status=none"
    )
    machine.succeed("sync")
    machine.succeed("sleep 2")

with subtest("Stop monitor and print captured block events"):
    stop_udev_monitor(monitor_pid)
    raw = read_monitor_output()
    print(f"Raw udev monitor output:\n{raw}")

    blocks = split_event_blocks(raw)
    print_event_blocks("All captured block-event blocks", blocks)

    candidates = [
        victim["by_id"],
        victim_devname,
        victim["label"],
        f"virtio-{victim['label']}",
        victim["mapper"],
        f"DM_NAME={victim['label']}",
        "ACTION=remove",
        "ACTION=change",
    ]
    filtered = relevant_blocks(blocks, candidates)
    print_event_blocks("Filtered blocks related to removed disk or candidate signals", filtered)

    parsed = [parse_event_block(block) for block in filtered]
    print("Parsed filtered event summaries:")
    if not parsed:
        print("(none)")
    for event in parsed:
        print(
            {
                key: event.get(key)
                for key in [
                    "ACTION",
                    "SUBSYSTEM",
                    "DEVNAME",
                    "DEVPATH",
                    "DEVTYPE",
                    "ID_SERIAL",
                    "ID_SERIAL_SHORT",
                    "ID_PATH",
                    "DM_NAME",
                ]
            }
        )

    remove_events = [e for e in parsed if is_victim_remove(e)]
    assert len(remove_events) > 0, "Expected udev ACTION=remove event for victim disk"

machine.shutdown()
