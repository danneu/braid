# Test: braid-enroll-generate
#
# Intent: Verify that `braid enroll --generate` atomically creates a
# 4096-byte keyfile with mode 400, enrolls it into all pool disks, and that
# generated keyfile can unlock the pool. Also verifies --generate refuses to
# overwrite an existing keyfile.
#
# Why it exists: The --generate flag replaces the manual dd/chmod workflow.
# If the keyfile is created before preflight validation (e.g., wrong
# passphrase), a useless keyfile is left behind. The two-phase approach
# (validate first, generate only on success) prevents this.
#
# Scenario: 2-disk RAID1 pool. --generate creates keyfile and enrolls.
# Lock, unlock with generated keyfile. --generate refuses overwrite.
# Slot conflict prevents keyfile creation.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for k in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{k} 2>/dev/null || true")


# --- Setup: Create 2-disk RAID1 pool ---

with subtest("Setup: create 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'generate test data' > /mnt/storage/gentest.txt")
    machine.succeed("sync")

# --- Test 1: --generate creates keyfile and enrolls ---

with subtest("Test 1: --generate creates keyfile and enrolls into all disks"):
    machine.succeed("mkdir -p /tmp/usb")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

    # Verify keyfile exists with correct size and permissions
    machine.succeed("test -f /tmp/usb/braid.key")
    size = machine.succeed("stat -c %s /tmp/usb/braid.key").strip()
    assert size == "4096", f"Expected keyfile size 4096, got {size}"
    mode = machine.succeed("stat -c %a /tmp/usb/braid.key").strip()
    assert mode == "400", f"Expected mode 400, got {mode}"

    # Verify slot 1 is occupied on both disks
    for dev in ["virtio-disk1", "virtio-disk2"]:
        dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/{dev}")
        assert '"1"' in dump, f"slot 1 not found in luksDump for {dev}: {dump}"

# --- Test 2: Lock, then unlock with generated keyfile ---

with subtest("Test 2: unlock with generated keyfile"):
    close_all()

    machine.fail("mountpoint -q /mnt/storage")

    machine.succeed("braid unlock --key-file /tmp/usb/braid.key")

    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/gentest.txt").strip()
    assert content == "generate test data", f"Expected 'generate test data', got '{content}'"

# --- Test 3: --generate refuses to overwrite existing keyfile ---

with subtest("Test 3: --generate refuses overwrite"):
    pq = shlex.quote(passphrase)
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

# --- Test 4: Slot conflict prevents keyfile creation ---

with subtest("Test 4: slot conflict prevents keyfile creation"):
    close_all()

    # Remove keyfile from both disks and delete existing keyfile
    for dev in ["virtio-disk1", "virtio-disk2"]:
        machine.succeed(f"cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/{dev} 1")
    machine.succeed("rm /tmp/usb/braid.key")

    # Put an unknown key into disk2's slot 1
    machine.succeed("dd if=/dev/urandom of=/tmp/conflict.key bs=32 count=1 iflag=fullblock")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"cryptsetup luksAddKey --key-slot 1 /dev/disk/by-id/virtio-disk2 /tmp/conflict.key"
    )

    # --generate should fail due to slot conflict, and keyfile must NOT be created
    machine.fail(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin"
    )

    # Verify keyfile was NOT created (preflight prevented generation)
    machine.fail("test -f /tmp/usb/braid.key")

machine.shutdown()
