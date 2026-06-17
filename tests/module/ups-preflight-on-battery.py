# Test: ups-preflight-on-battery
#
# Intent: preflight refusal on battery blocks mutation starts end-to-end,
# not just in unit tests. The refusal must land on the representative
# entry point (`braid add`) as a Validation-shaped error, with no
# pending-op.json written to disk.
#
# Why it exists: one of the two shipped v1 safety guarantees is that
# `braid add / remove / remove-missing / replace` refuse to begin while
# the UPS is on battery. Unit tests over MockRunner cover the decision
# logic per-entry-point, but the live wiring -- `config.ups` plumbed
# from /etc/braid/config.json, CmdRequest::UpscQuery dispatched to the
# wrapper's PATH, `upsc` against real NUT -- has to agree too. A silent
# breakage in any of those layers would make the guarantee hollow
# without any unit-test evidence.
#
# Scenario: operator runs `braid add disk1` while the UPS is already on
# battery (outage already started). braid refuses before any journal
# write or LUKS operation.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

# Wait for the NUT chain: secrets oneshot, then upsd + upsmon. upsdrv
# has Type=oneshot + RemainAfterExit=true, so is-active stays "active"
# after the driver register call returns.
machine.wait_for_unit("braid-ups-secrets.service", timeout=60)
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)

with subtest("Preflight precondition: upsc reports OB"):
    # OB (on battery) without LB keeps upsmon's critical-state trigger
    # silent (see reference/nut/clients/upsmon.c:1404 -- critical
    # requires ST_ONBATT AND ST_LOWBATT) so the test VM does not shut
    # down before the assertions run. braid preflight refuses on OB
    # alone, so this is enough to exercise the refusal path.
    machine.wait_until_succeeds(
        "upsc ups 2>/dev/null | grep -q '^ups.status: OB$'", timeout=30
    )

with subtest("braid add refuses with Validation error on battery"):
    # Keep the passphrase trivial; we expect the command to exit before
    # it ever touches the disk, so the passphrase is never actually
    # used. Use a very low PBKDF for completeness in case early
    # validation probes hit cryptsetup before the UPS check (it
    # shouldn't, but the cost of being defensive here is one env var).
    rc, out = machine.execute(
        "echo -n 'testpass' | braid add "
        "disk1=/dev/disk/by-id/virtio-disk1 "
        "--passphrase-stdin --yes 2>&1"
    )
    assert rc != 0, f"braid add must fail when UPS is on battery; stdout:\n{out}"
    assert "utility power" in out, (
        f"error output should mention utility power; got:\n{out}"
    )
    assert "braid ups status" in out, (
        f"error output should hint at 'braid ups status'; got:\n{out}"
    )

with subtest("Preflight refusal leaves no pending-op journal"):
    # The on-battery check runs before journal::write_journal in
    # cli/src/add.rs, so even a refused add must not have created a
    # pending-op.json. If this file appears, the check has drifted to
    # run after the journal write.
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("braid ups status shows the same OB set the preflight saw"):
    out = machine.succeed("braid ups status")
    assert "Status: OB  [warn] on battery" in out.splitlines(), (
        f"braid ups status should report tagged OB severity; got:\n{out}"
    )

machine.shutdown()
