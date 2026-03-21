# Repro: udev events on quiet missing-disk event
#
# Intent: Observe what raw block-device udev monitoring reports when one member
# of a mounted LUKS+btrfs RAID1 pool disappears and no subsequent filesystem
# I/O is forced.
#
# Why it exists: If braid ever uses udev as a passive source for
# "configured pool drive disappeared", it needs evidence that a useful block
# event appears immediately on disappearance without probing disks.
#
# Scenario: A 3-disk RAID1 pool is mounted normally. QEMU deletes the host
# backing drive for one member while the guest-visible block device remains
# present. A background `udevadm monitor` capture runs before injection and is
# stopped shortly after. The test prints all captured block events plus a
# filtered subset related to the removed disk, but does not require any match.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
mount = "/mnt/storage"
log_path = "/tmp/udev-monitor.log"
victim_by_id = "/dev/disk/by-id/virtio-disk3"
disks = ["disk1", "disk2", "disk3"]


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

with subtest("Start background udev monitor before disappearance injection"):
    victim_devname = machine.succeed(f"readlink -f {victim_by_id}").strip()
    print(f"Victim by-id path: {victim_by_id}")
    print(f"Victim raw device path before disappearance: {victim_devname}")
    monitor_pid = start_udev_monitor()
    print(f"udevadm monitor pid: {monitor_pid}")

with subtest("Delete one backing drive while mounted, with no follow-up I/O"):
    delete_backing_drive("empty2.qcow2")
    machine.succeed("sleep 2")

with subtest("Pool remains mounted after backing drive deletion"):
    machine.succeed(f"mountpoint -q {mount}")

with subtest("Stop monitor and print captured block events"):
    stop_udev_monitor(monitor_pid)
    raw = read_monitor_output()
    print(f"Raw udev monitor output:\n{raw}")

    blocks = split_event_blocks(raw)
    print_event_blocks("All captured block-event blocks", blocks)

    candidates = [
        victim_by_id,
        victim_devname,
        "disk3",
        "virtio-disk3",
        "/dev/mapper/disk3",
        "DM_NAME=disk3",
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

machine.shutdown()
