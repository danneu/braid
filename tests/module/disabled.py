# Test: braid module is inert when disabled
#
# Intent: Verify that importing the braid module with enable = false
#   (the default) produces no braid systemd units and no mount.
#
# Why it exists: The module gate (lib.mkIf cfg.enable) could leak a
#   unit definition and silently activate services on non-braid machines.
#   A mountpoint-only check would miss partial activation.
#
# Scenario: A NixOS machine imports the braid module but never sets
#   braid.enable = true. The machine should boot cleanly with zero
#   braid-* unit files installed.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Module is inert when disabled"):
    machine.succeed("uname -a")
    machine.fail("mountpoint /mnt/storage")
    # The disabled module must leak no braid-* units. NixOS renders every
    # module-defined unit and any activation symlink into /etc/systemd/system,
    # so this static check avoids querying PID 1 through D-Bus while it is
    # still settling after boot.
    leaked = machine.succeed("find /etc/systemd/system -name 'braid-*'").strip()
    assert leaked == "", f"disabled module leaked braid units:\n{leaked}"

machine.shutdown()
