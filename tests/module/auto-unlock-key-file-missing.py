# Test: auto-unlock-key-file-missing
#
# Intent: Verify that auto-unlock skips cleanly when the USB is present but
# braid.key is missing.
#
# Why it exists: The realpath -e missing-file path must still unmount via the
# EXIT trap, leave the pool locked, and keep boot healthy.
#
# Scenario: Boot with autoUnlock enabled and an attached USB filesystem that
# contains no braid.key. The service logs the missing keyfile, exits 0, and
# leaves /run/braid-key/mnt unmounted.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
machine.wait_until_succeeds(
    "systemctl show -p ActiveState --value braid-auto-unlock.service | grep -x inactive",
    timeout=120,
)

with subtest("Auto-unlock service completed successfully"):
    result = machine.succeed(
        "systemctl show -p Result --value braid-auto-unlock.service"
    ).strip()
    assert result == "success", f"Expected service result success, got {result}"

with subtest("Journal explains missing keyfile"):
    journal = machine.succeed("journalctl -u braid-auto-unlock.service --no-pager")
    assert "keyfile not found at /run/braid-key/mnt/braid.key" in journal, \
        f"Expected missing-keyfile message in journal, got:\n{journal}"

with subtest("Pool is NOT mounted"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("USB is unmounted after missing keyfile skip"):
    ret = machine.execute("mountpoint -q /run/braid-key/mnt")
    assert ret[0] != 0, "USB should NOT be mounted at /run/braid-key/mnt"

machine.shutdown()
