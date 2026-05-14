# Intent: verify Browse > NUT > UPSes runs even when braid.ups.enable is false.
#
# Why it exists: UPS discovery is the bootstrap path for finding the UPS name
# to put in braid.ups.name, so it must not be blocked by missing UPS config or
# a wrapper PATH that lacks NUT client tools.
#
# Scenario: user installs braid, has not enabled UPS support, opens the TUI,
# and checks NUT > UPSes to discover configured devices.

import time


def press(key):
    machine.send_key(key)
    time.sleep(0.1)

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("discover pool membership without unlocking"):
    machine.succeed("braid discover --write")

with subtest("launch live tui through the installed module wrapper"):
    machine.succeed("systemctl start braid-tui-canary.service")
    machine.wait_for_unit("braid-tui-canary.service")
    machine.succeed("chvt 2")
    machine.wait_until_tty_matches("2", r"Data\s+Scrub\s+Browse")

with subtest("navigate to Browse > NUT > UPSes"):
    press("tab")
    press("tab")
    machine.wait_until_tty_matches("2", r"Pgm")
    press("j")
    machine.wait_until_tty_matches("2", r"UPS not configured")
    press("l")
    for _ in range(5):
        press("j")

    machine.wait_until_tty_matches("2", r">\s+UPSes")
    machine.wait_until_tty_matches("2", r"(Error:|Connection failure)")
    screen = machine.get_tty_text("2")
    assert "UPS not configured" not in screen, screen
    assert "No such file or directory" not in screen, screen
    assert "not found" not in screen, screen

machine.succeed("systemctl stop braid-tui-canary.service")
machine.shutdown()
