# Test: auto-unlock-key-wrong
#
# Intent: Verify that when autoUnlock is enabled and a USB device is
# present but contains a wrong/invalid keyfile, boot succeeds with the
# pool still functional.
#
# Why it exists: A corrupted or swapped USB must not block boot, cause
# error loops, or leave the system in a degraded state.
#
# Scenario: USB disk is present but keyfile has wrong content (random
# bytes, not enrolled in pool). Boot completes. System is functional.
# Warning in journal.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("System booted successfully"):
    machine.succeed("true")

with subtest("Pool is NOT mounted (wrong keyfile)"):
    machine.fail("mountpoint -q /mnt/storage")

with subtest("Auto-unlock service completed (not failed)"):
    # The service should exit 0 even with wrong keyfile (graceful skip)
    ret = machine.execute("systemctl is-failed braid-auto-unlock.service")
    assert ret[0] != 0, "braid-auto-unlock should NOT be in failed state"

with subtest("Journal has warning about failed unlock"):
    journal = machine.succeed("journalctl -u braid-auto-unlock.service --no-pager 2>/dev/null || true")
    print(f"Auto-unlock journal:\n{journal}")
    # The service should log something about the failure
    assert "unlock failed" in journal or "wrong keyfile" in journal or "skipping" in journal, \
        f"Expected warning in journal about failed unlock, got:\n{journal}"

with subtest("USB is unmounted after auto-unlock attempt"):
    ret = machine.execute("mountpoint -q /run/braid-key/mnt")
    assert ret[0] != 0, "USB should NOT be mounted at /run/braid-key/mnt"

machine.shutdown()
