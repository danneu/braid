# Intent: verify the `braid tui` Browse tab exercises live btrfs subvolume
# list parsing and subvolume detail drill-in on a real btrfs pool.
#
# Why it exists: Browse moved into the TUI; the parser canary must cover the
# real PTY-driven integration path.
#
# Scenario: user opens `sudo braid tui`, tabs to Browse, selects Btrfs
# Subvolumes, drills into a row, and backs out to the list.

import re
import time


def press(key):
    machine.send_key(key)
    time.sleep(0.1)


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("discover and unlock pool"):
    machine.succeed("braid discover --write")
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("create subvolume for drill-in test"):
    machine.succeed("btrfs subvolume create /mnt/storage/test-subvol")

with subtest("launch live tui on tty2"):
    machine.succeed("systemctl start braid-tui-canary.service")
    machine.wait_for_unit("braid-tui-canary.service")
    machine.succeed("chvt 2")
    machine.wait_until_tty_matches("2", r"Data\s+Scrub\s+Browse")

with subtest("navigate to Browse > Btrfs > Subvolumes"):
    press("tab")
    press("tab")
    machine.wait_until_tty_matches("2", r"Pgm")
    press("l")
    press("j")
    press("j")
    machine.wait_until_tty_matches("2", r"test-subvol")

with subtest("drill into subvolume detail and return"):
    press("l")
    press("l")
    press("ret")
    machine.wait_until_tty_matches("2", r"Name:\s+test-subvol")
    press("esc")
    machine.wait_until_tty_matches("2", r"test-subvol")

with subtest("browse systemd status detail"):
    press("h")
    press("h")
    press("h")
    press("j")
    press("j")
    press("l")
    press("l")
    # Picker row 0 is braid-auto-unlock.service: a not-found phantom that
    # `list-units --all` shows because storage.nix orders a real unit `before=`
    # it while autoUnlock is disabled here. Its status has no Loaded: line, so
    # drilling into row 0 would never satisfy the wait below. Drive the
    # selection down to braid-online.service (pool-online sentinel,
    # active/exited once mounted) instead, order-independently, and self-check
    # the `>` marker before ret so a future picker reorder fails loudly rather
    # than silently testing the wrong unit.
    machine.wait_until_tty_matches("2", r"braid-online\.service", timeout=30)
    for _ in range(15):
        if re.search(r">\s+braid-online", machine.get_tty_text("2")):
            break
        press("j")
    machine.wait_until_tty_matches("2", r">\s+braid-online", timeout=30)
    press("ret")
    machine.wait_until_tty_matches("2", r"Loaded:", timeout=30)
    press("esc")
    machine.wait_until_tty_matches("2", r"braid-online\.service", timeout=30)

with subtest("browse smart health detail"):
    press("h")
    press("h")
    press("j")
    press("l")
    press("j")
    press("l")
    machine.wait_until_tty_matches("2", r"disk1")
    press("ret")
    # disk1 is a present, unlocked member, so the SMART detail/footer dispatches
    # against the live backing node (decision 024), not the persisted by-id
    # handle. `/dev/vd` matches whichever virtio node cryptsetup reports.
    machine.wait_until_tty_matches("2", r"/dev/vd")
    press("esc")
    machine.wait_until_tty_matches("2", r"disk1")

with subtest("browse lsblk filesystems"):
    press("h")
    press("h")
    press("j")
    press("l")
    press("j")
    machine.wait_until_tty_matches("2", r"btrfs")

machine.succeed("systemctl stop braid-tui-canary.service")
machine.shutdown()
