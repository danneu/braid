# Test: auto-unlock-key-missing
#
# Intent: Verify that when autoUnlock is enabled but no USB device is
# present, boot succeeds normally with the pool locked.
#
# Why it exists: Principle 1 (resilient by default). A missing USB key
# must NEVER block boot or cause systemd to enter degraded state.
#
# Scenario: Same module config as key-present test, but no usbkey virtual
# disk attached. VM boots to multi-user. Pool is NOT mounted. System is
# SSH-accessible and functional.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("System booted successfully"):
    # Basic smoke test — system is functional
    machine.succeed("true")

with subtest("Pool is NOT mounted (no USB key)"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("Auto-unlock service did not fail"):
    # The service should have exited 0 (skipped because USB not present)
    ret = machine.execute("systemctl is-failed braid-auto-unlock.service")
    assert ret[0] != 0, "braid-auto-unlock should NOT be in failed state"

machine.shutdown()
