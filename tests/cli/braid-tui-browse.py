# Intent: verify the `braid tui` Browse tab exercises live btrfs subvolume
# list parsing and subvolume detail drill-in on a real btrfs pool.
#
# Why it exists: Browse moved into the TUI; the parser canary must cover the
# real PTY-driven integration path.
#
# Scenario: user opens `sudo braid tui`, tabs to Browse, selects Btrfs
# Subvolumes, drills into a row, and backs out to the list.

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
    machine.send_key("tab")
    machine.send_key("tab")
    machine.wait_until_tty_matches("2", r"Browse")
    machine.send_key("l")
    machine.send_key("j")
    machine.send_key("j")
    machine.wait_until_tty_matches("2", r"test-subvol")

with subtest("drill into subvolume detail and return"):
    machine.send_key("l")
    machine.send_key("ret")
    machine.wait_until_tty_matches("2", r"Name:\s+test-subvol")
    machine.send_key("esc")
    machine.wait_until_tty_matches("2", r"test-subvol")

machine.succeed("systemctl stop braid-tui-canary.service")
machine.shutdown()
