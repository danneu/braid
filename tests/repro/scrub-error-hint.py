# Repro: braid status scrub error hint
#
# Intent: Trigger a scrub-visible data-extent read failure and exercise the
# exact journalctl command printed by `braid status`.
#
# Why it exists: Non-zero scrub counts are only actionable if the operator can
# find the kernel's best-effort detail lines without braid parsing the journal.
#
# Scenario: One virtio disk is wrapped in dm-dust below LUKS+btrfs. A victim
# file is written while dm-dust is healthy, its first physical block is marked
# bad, then `btrfs scrub start -B` reports an uncorrectable error. `braid
# status` must print a scrub-specific journalctl command, and that command must
# surface both the repair-summary line and the path-bearing detail line.

import re


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
raw_disk = "/dev/disk/by-id/virtio-disk1"
dust = "/dev/mapper/dust1"
mapper = "disk1"
mount = "/mnt/storage"
victim = f"{mount}/victim.bin"
grep_pattern = "BTRFS.*(at logical.*on (dev|mirror)|super block at physical)"


def dm_table():
    sectors = machine.succeed(f"blockdev --getsz {raw_disk}").strip()
    return f"0 {sectors} dust {raw_disk} 0 4096"


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


def printed_journal_command(status_output):
    for line in status_output.splitlines():
        command = line.strip()
        if command.startswith("sudo journalctl -k --since "):
            return command
    raise AssertionError(f"Could not find printed journalctl command:\n{status_output}")


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
    machine.succeed(f"dmsetup message dust1 0 addbadblock {block}")
    machine.succeed("dmsetup message dust1 0 enable")
    machine.succeed("sync")
    machine.succeed("echo 3 > /proc/sys/vm/drop_caches")

with subtest("Scrub reports the injected block as uncorrectable"):
    status, output = machine.execute(f"btrfs scrub start -B {mount} 2>&1")
    print(f"btrfs scrub exit status: {status}")
    print(f"btrfs scrub output:\n{output}")
    assert status == 3, f"Expected scrub exit 3 for uncorrectable errors, got {status}: {output}"

with subtest("braid status prints a scrub-specific journalctl hint"):
    status, output = machine.execute("braid status 2>&1")
    print(f"braid status exit status: {status}")
    print(f"braid status output:\n{output}")
    assert status == 0, f"Expected braid status to succeed, got {status}: {output}"
    assert "scrub error details:" in output, f"Expected scrub hint label in status output:\n{output}"

    command = printed_journal_command(output)
    assert command.startswith("sudo journalctl -k --since '"), command
    assert command.endswith(f"--grep '{grep_pattern}'"), command

with subtest("Printed journalctl command surfaces scrub detail lines"):
    journal = machine.succeed(command)
    print(f"journalctl output from printed command:\n{journal}")
    lines = {line.strip() for line in journal.splitlines() if line.strip()}
    assert len(lines) >= 2, f"Expected at least two distinct journal lines, got:\n{journal}"
    assert re.search(
        r"unable to fixup \(regular\) error at logical .* on dev .* physical",
        journal,
    ), f"Expected uncorrectable scrub repair-summary line, got:\n{journal}"
    assert "(path: victim.bin)" in journal, f"Expected path-bearing scrub detail line, got:\n{journal}"

with subtest("Cleanup"):
    machine.execute(f"umount {mount}")
    machine.execute(f"cryptsetup close {mapper}")
    machine.execute("dmsetup remove dust1")

machine.shutdown()
