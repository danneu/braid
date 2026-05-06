# Test: auto-unlock-key-file-symlink
#
# Intent: Verify that auto-unlock refuses a braid.key symlink that resolves
# outside the USB mount root.
#
# Why it exists: The USB filesystem is attacker-controlled. A symlink escape
# must not let auto-unlock read a host file as key material.
#
# Scenario: Boot with autoUnlock enabled and an attached USB filesystem whose
# braid.key points to /etc/shadow. The service logs the refusal, exits 0, and
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

with subtest("Journal explains symlink refusal"):
    journal = machine.succeed("journalctl -u braid-auto-unlock.service --no-pager")
    assert "keyfile resolves outside mount root" in journal, \
        f"Expected symlink refusal in journal, got:\n{journal}"

with subtest("Pool is NOT mounted"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("USB is unmounted after symlink refusal"):
    ret = machine.execute("mountpoint -q /run/braid-key/mnt")
    assert ret[0] != 0, "USB should NOT be mounted at /run/braid-key/mnt"

machine.shutdown()
