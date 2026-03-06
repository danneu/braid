# Repro: cryptsetup close fails when filesystem is mounted on the LUKS device
#
# Intent: Prove that `cryptsetup close` fails (non-zero exit, stderr mentions
# "busy" or "in use") when a filesystem is mounted on the LUKS device. After
# umount, close succeeds.
#
# Why it exists: Documents the raw kernel behavior that causes `braid lock` to
# fail if umount is skipped or fails. This is the baseline for the improved
# error handling in `braid lock`.
#
# Scenario: Single LUKS device with btrfs mounted. Attempt to close the LUKS
# mapper while the filesystem is still mounted — observe failure. Then umount
# and close succeeds.

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_format = "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"

with subtest("Setup: LUKS format, open, mkfs, mount"):
    dev = "/dev/disk/by-id/virtio-disk1"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat {luks_format} {dev}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} disk1")
    machine.succeed("mkfs.btrfs -f /dev/mapper/disk1")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")
    machine.succeed("echo 'test data' > /mnt/storage/test.txt")
    machine.succeed("sync")

with subtest("cryptsetup close fails while mounted"):
    exit_code, stderr = machine.execute("cryptsetup close disk1 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Stderr: {stderr}")
    assert exit_code != 0, f"Expected cryptsetup close to fail while mounted, but exit was {exit_code}"
    stderr_lower = stderr.lower()
    assert "busy" in stderr_lower or "in use" in stderr_lower, \
        f"Expected 'busy' or 'in use' in error output, got: {stderr}"

with subtest("After umount, cryptsetup close succeeds"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close disk1")
    machine.fail("test -e /dev/mapper/disk1")

machine.shutdown()
