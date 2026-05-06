# Test: auto-unlock-runtime-dir-mode
#
# Intent: Verify that /run/braid-key remains 0700 root:root while the USB is
# mounted at /run/braid-key/mnt, so non-root users cannot traverse to it.
#
# Why it exists: The mounted USB filesystem can have permissive root
# permissions. The parent directory, not the mounted inode, is the security
# boundary that protects the plaintext key during the mount window.
#
# Scenario: Boot with autoUnlock enabled and a vfat USB attached. Start the
# mount unit directly so the test can observe the mounted state, verify nobody
# cannot list the parent or child path, then stop the mount unit.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
machine.wait_until_succeeds(
    "systemctl show -p ActiveState --value braid-auto-unlock.service | grep -x inactive",
    timeout=120,
)

try:
    with subtest("USB can be mounted directly for observation"):
        machine.succeed("systemctl start 'run-braid\\x2dkey-mnt.mount'")
        machine.succeed("mountpoint -q /run/braid-key/mnt")

    with subtest("Locked parent mode is preserved while USB is mounted"):
        stat = machine.succeed("stat -c '%a %U %G' /run/braid-key").strip()
        assert stat == "700 root root", f"Expected 700 root root, got {stat}"

    with subtest("Non-root cannot traverse to mounted USB"):
        ret = machine.execute("runuser -u nobody -- ls /run/braid-key")
        assert ret[0] != 0, "nobody should not be able to list /run/braid-key"

        ret = machine.execute("runuser -u nobody -- ls /run/braid-key/mnt")
        assert ret[0] != 0, "nobody should not be able to list /run/braid-key/mnt"
finally:
    machine.execute("systemctl stop 'run-braid\\x2dkey-mnt.mount'")

with subtest("USB is unmounted after direct observation"):
    ret = machine.execute("mountpoint -q /run/braid-key/mnt")
    assert ret[0] != 0, "USB should NOT be mounted at /run/braid-key/mnt"

machine.shutdown()
