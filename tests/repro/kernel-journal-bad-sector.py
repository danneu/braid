# Repro: kernel journal on bad-sector-style read failure
#
# Intent: Trigger a real read-side bad-block failure below a LUKS+btrfs stack
# and inspect the resulting kernel journal entries as structured JSON.
#
# Why it exists: braid should not guess what the journal looks like for
# medium-error-style failures. This test captures what the pinned stack logs
# when a known block becomes unreadable after data is already written.
#
# Scenario: One virtio disk is wrapped in a dm-dust target. A victim file is
# written while dm-dust is in bypass mode, then `filefrag` is used to locate
# the first physical block of the file. That block is marked bad in dm-dust,
# read failures are enabled, page cache is dropped, and a direct read of the
# file is attempted. The test then reads the post-marker kernel journal and
# prints the relevant entries for analysis.

import json
import re


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
raw_disk = "/dev/disk/by-id/virtio-disk1"
dust = "/dev/mapper/dust1"
mapper = "disk1"
mount = "/mnt/storage"
victim = f"{mount}/victim.bin"
marker = "BRAID_REPRO_BAD_SECTOR_START"


def dm_table():
    sectors = machine.succeed(f"blockdev --getsz {raw_disk}").strip()
    return f"0 {sectors} dust {raw_disk} 0 4096"


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


def first_physical_block(path):
    out = machine.succeed(f"filefrag -v -b4096 {path}")
    print(f"filefrag output for {path}:\n{out}")
    for line in out.splitlines():
        m = re.match(r"^\s*0:\s+\d+\.\.\s*\d+:\s+(\d+)\.\.\s*\d+:", line)
        if m:
            return int(m.group(1))
    raise AssertionError(f"Could not parse first physical block from filefrag output:\n{out}")


def luks_payload_offset_blocks():
    out = machine.succeed(f"cryptsetup status {mapper}")
    print(f"cryptsetup status for {mapper}:\n{out}")
    m = re.search(r"offset:\s+(\d+)\s+\[512-byte units\]", out)
    if not m:
        raise AssertionError(f"Could not parse payload offset from cryptsetup status:\n{out}")
    sectors = int(m.group(1))
    assert sectors % 8 == 0, f"Expected 4K-aligned payload offset, got {sectors} sectors"
    return sectors // 8


with subtest("Setup: dm-dust bypass, then LUKS format/open, mkfs, mount"):
    machine.succeed("modprobe dm-dust")
    machine.succeed(f"dmsetup create dust1 --table '{dm_table()}'")
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- "
        f"--pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dust}"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dust} {mapper}"
    )
    machine.succeed(f"mkfs.btrfs -f -d single -m dup /dev/mapper/{mapper}")
    machine.succeed(f"mkdir -p {mount}")
    machine.succeed(f"mount /dev/mapper/{mapper} {mount}")

with subtest("Write victim file while dm-dust is in bypass mode"):
    machine.succeed(f"dd if=/dev/zero of={victim} bs=4K count=256 conv=fsync status=none")
    machine.succeed("sync")

with subtest("Mark victim's first physical block bad and enable read failures"):
    file_block = first_physical_block(victim)
    payload_offset = luks_payload_offset_blocks()
    block = payload_offset + file_block
    print(f"victim first physical block on decrypted mapper: {file_block}")
    print(f"LUKS payload offset in 4K blocks: {payload_offset}")
    print(f"bad block on raw disk for dm-dust: {block}")
    machine.succeed(f"printf '<6>{marker}\\n' > /dev/kmsg")
    machine.succeed(f"dmsetup message dust1 0 addbadblock {block}")
    machine.succeed("dmsetup message dust1 0 enable")
    machine.succeed("sync")
    machine.succeed("echo 3 > /proc/sys/vm/drop_caches")

with subtest("Direct read through mounted file fails"):
    status, output = machine.execute(
        f"dd if={victim} of=/dev/null bs=4K count=1 iflag=direct status=none 2>&1"
    )
    print(f"dd exit status: {status}")
    print(f"dd output:\n{output}")
    assert status != 0, f"Expected direct read to fail under dm-dust, got exit 0: {output}"

with subtest("Kernel journal after marker contains bad-sector evidence"):
    entries = kernel_entries_after_marker()
    assert entries, "Expected kernel journal entries after repro marker"

    interesting = []
    for entry in entries:
        msg = entry.get("MESSAGE", "")
        if (
            "device-mapper: dust:" in msg
            or "BTRFS error" in msg
            or "I/O error" in msg
            or "Buffer I/O error" in msg
            or "critical medium error" in msg
            or "badblock" in msg
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

    assert interesting, "Expected at least one relevant kernel journal entry after bad block read"

with subtest("Cleanup"):
    machine.execute(f"umount {mount}")
    machine.execute(f"cryptsetup close {mapper}")
    machine.execute("dmsetup remove dust1")

machine.shutdown()
