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
    machine.succeed("mkfs.btrfs -f -d single -m dup /dev/mapper/disk1")
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/disk1 /mnt/storage")
    machine.succeed("echo 'test data' > /mnt/storage/test.txt")
    machine.succeed("sync")

with subtest("cryptsetup close fails while mounted"):
    exit_code, stderr = machine.execute("cryptsetup close disk1 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Stderr: {stderr}")
    # EBUSY -> translate_errno -> exit 5. Busy classification in
    # cli/src/mapper_close.rs close_mapper_with_retry relies on this
    # exact code (wording-independent). Do not relax this assertion to
    # != 0.
    assert exit_code == 5, \
        f"Expected exit 5 (EBUSY) while mounted, got {exit_code}. stderr: {stderr}"
    # Descriptive wording check -- not load-bearing for braid's classifier
    # anymore, but a wording shift is still worth surfacing in the log.
    stderr_lower = stderr.lower()
    assert "busy" in stderr_lower or "in use" in stderr_lower, \
        f"Expected 'busy' or 'in use' in error output, got: {stderr}"

with subtest("After umount, cryptsetup close succeeds"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close disk1")
    machine.fail("test -e /dev/mapper/disk1")

with subtest("cryptsetup close on already-closed mapper returns ENODEV (exit 4)"):
    # Pins the non-busy distractor exit code that lock.rs unit tests
    # model (see `lock_mapper_close_fatal_when_umount_succeeded` and
    # siblings). If cryptsetup ever started returning exit 5 here,
    # close_mapper_with_retry would misclassify a fatal error as busy
    # and spin three retries before surfacing it.
    exit_code, stderr = machine.execute("cryptsetup close disk1 2>&1")
    print(f"Exit code: {exit_code}")
    print(f"Stderr: {stderr}")
    assert exit_code == 4, \
        f"Expected exit 4 (ENODEV) for already-closed mapper, got {exit_code}. stderr: {stderr}"

machine.shutdown()
