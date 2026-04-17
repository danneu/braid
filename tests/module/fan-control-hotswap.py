# Test: fan-control-hotswap
#
# Intent: Verify that the udev-triggered reload chain works end-to-end: the
# braid-fan-reload oneshot, when started, restarts hddfancontrol-braid after
# the debounce sleep. Invoking the oneshot is exactly what the production
# udev rules do on SATA add/remove (via SYSTEMD_WANTS and RUN+=systemctl),
# so this exercises the same path the rules drive at runtime.
#
# Why it exists: The udev rule -> oneshot -> daemon restart chain is the
# most failure-prone part of fan control. A wiring-only test (grep rules
# files) verifies the rules' text but cannot catch breakage in the oneshot's
# ExecStartPre/ExecStart or in the daemon's restart semantics.
#
# The production udev rules (verified textually in the wiring test) cannot be
# exercised with a real SATA device inside a nixosTest VM -- the NixOS test
# framework's virtualisation.emptyDiskImages wires every drive as
# virtio-blk-pci, which surfaces as /dev/vdX with ID_BUS != "ata", so the
# ID_BUS=="ata" filter never matches. Bypassing that would require custom
# AHCI QEMU wiring that's hard to make reliable across architectures.
#
# Scenario: NixOS VM with braid.fanControl enabled and the daemon stubbed
# (sleep infinity) since there are no real hwmon/PWM devices in the VM.
# The test triggers braid-fan-reload via systemctl -- the same action the
# udev rules perform -- and verifies the daemon restarts (observed via
# ActiveEnterTimestamp change).

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("hddfancontrol-braid is running (stub)"):
    machine.succeed("systemctl is-active hddfancontrol-braid.service")

with subtest("starting braid-fan-reload restarts hddfancontrol-braid (add-path equivalent)"):
    # This mirrors what the production udev add rule does via
    # SYSTEMD_WANTS+=braid-fan-reload.service.
    ts_before = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    machine.succeed("systemctl start braid-fan-reload.service")
    ts_after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    assert ts_before != ts_after, (
        f"Daemon was not restarted by braid-fan-reload.\n"
        f"Before: {ts_before}\nAfter:  {ts_after}"
    )

with subtest("starting braid-fan-reload --no-block restarts daemon (remove-path equivalent)"):
    # This mirrors what the production udev remove rule does via
    # RUN+="systemctl start --no-block braid-fan-reload.service".
    ts_before = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    machine.succeed("systemctl start --no-block braid-fan-reload.service")
    # Debounce sleep in ExecStartPre is 5s; wait past it plus restart time.
    machine.sleep(8)
    ts_after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    assert ts_before != ts_after, (
        f"Daemon was not restarted by --no-block braid-fan-reload.\n"
        f"Before: {ts_before}\nAfter:  {ts_after}"
    )

with subtest("daemon is healthy after two restarts"):
    machine.succeed("systemctl is-active hddfancontrol-braid.service")

machine.shutdown()
