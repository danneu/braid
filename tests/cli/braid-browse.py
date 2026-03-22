# Intent: verify braid browse --check exercises the full command pipeline on a
# real btrfs pool: filesystem usage, subvolume list + parse, and subvolume
# drill-in via btrfs subvolume show.
#
# Why it exists: the browse TUI depends on btrfs-progs output format; this
# catches format regressions that unit tests with fixtures cannot detect.
#
# Scenario: user runs `braid browse --check` after unlocking a pool that has
# at least one subvolume — the check proves all browse commands work.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("unlock pool"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("create subvolume for drill-in test"):
    machine.succeed("btrfs subvolume create /mnt/storage/test-subvol")

with subtest("braid browse --check exercises command pipeline"):
    output = machine.succeed("braid browse --check")
    print(f"browse --check output:\n{output}")
    assert "ok: btrfs filesystem usage" in output, f"missing filesystem usage: {output}"
    assert "ok: btrfs subvolume list" in output, f"missing subvolume list: {output}"
    assert "ok: btrfs subvolume show" in output, f"missing subvolume show: {output}"

machine.shutdown()
