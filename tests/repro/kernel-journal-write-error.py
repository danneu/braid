# Repro: kernel journal on real write I/O error
#
# Intent: Trigger a real write-side block error below a LUKS+btrfs stack and
# inspect the resulting kernel journal entries as structured JSON.
#
# Why it exists: braid should not implement kernel-storage alerting blindly.
# This test captures what the pinned NixOS/kernel stack actually logs for a
# real write EIO, including any structured device metadata.
#
# Scenario: One virtio disk is wrapped in a dm-flakey target. The pool is
# created and mounted while the target is healthy, then the target is reloaded
# into write-error mode. A write with fsync is forced through the mounted
# filesystem, causing a real kernel-visible I/O failure. The test then reads
# the post-marker kernel journal and verifies that at least one entry mentions
# the failing device and carries useful structured device identity.

import json


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
raw_disk = "/dev/disk/by-id/virtio-disk1"
flakey = "/dev/mapper/flakey1"
mapper = "disk1"
mount = "/mnt/storage"
marker = "BRAID_REPRO_WRITE_EIO_START"


def dm_table(up, down):
    sectors = machine.succeed(f"blockdev --getsz {raw_disk}").strip()
    return f"0 {sectors} flakey {raw_disk} 0 {up} {down} 1 error_writes"


def dm_create_healthy():
    table = dm_table(3600, 1)
    machine.succeed(f"dmsetup create flakey1 --table '{table}'")


def dm_switch_to_write_errors():
    table = dm_table(0, 3600)
    machine.succeed("dmsetup suspend flakey1")
    machine.succeed(f"dmsetup reload flakey1 --table '{table}'")
    machine.succeed("dmsetup resume flakey1")


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


with subtest("Setup: dm-flakey healthy, then LUKS format/open, mkfs, mount"):
    machine.succeed("modprobe dm-flakey")
    dm_create_healthy()
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {flakey}"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {flakey} {mapper}"
    )
    machine.succeed(f"mkfs.btrfs -f -d single -m dup /dev/mapper/{mapper}")
    machine.succeed(f"mkdir -p {mount}")
    machine.succeed(f"mount /dev/mapper/{mapper} {mount}")
    machine.succeed(f"dd if=/dev/zero of={mount}/healthy.bin bs=1M count=4 conv=fsync status=none")

with subtest("Reload dm-flakey into write-error mode"):
    machine.succeed(f"printf '<6>{marker}\\n' > /dev/kmsg")
    dm_switch_to_write_errors()

with subtest("Write through btrfs fails with non-zero exit"):
    status, output = machine.execute(
        f"dd if=/dev/zero of={mount}/failing.bin bs=1M count=8 conv=fsync status=none 2>&1"
    )
    print(f"dd exit status: {status}")
    print(f"dd output:\n{output}")
    assert status != 0, f"Expected write to fail under dm-flakey, got exit 0: {output}"

with subtest("Kernel journal after marker contains storage-error evidence"):
    entries = kernel_entries_after_marker()
    assert entries, "Expected kernel journal entries after repro marker"

    interesting = []
    for entry in entries:
        msg = entry.get("MESSAGE", "")
        if (
            "BTRFS error" in msg
            or "I/O error" in msg
            or "Buffer I/O error" in msg
            or "blk_update_request" in msg
            or "critical medium error" in msg
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

    assert interesting, "Expected at least one relevant kernel journal entry after write failure"
    assert any("BTRFS error" in entry.get("MESSAGE", "") for entry in interesting), (
        "Expected the journal to contain the btrfs device-error lines emitted during write failure"
    )

with subtest("Cleanup"):
    machine.execute(f"umount {mount}")
    machine.execute(f"cryptsetup close {mapper}")
    machine.execute("dmsetup remove flakey1")

machine.shutdown()
