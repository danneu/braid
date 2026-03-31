# Intent: verify braid discover's recovery workflow end-to-end.
#
# Why it exists: discover is the sole way to rebuild pool.json from labeled
# drives; a regression here leaves users unable to recover a lost pool config.
# Existing tests exercise discover --write as a setup step, but none cover
# read-only mode or the "pool.json already exists" guard.
#
# Scenario: user reinstalled NixOS on their NAS; pool.json is gone but drives
# retain their braid LUKS labels. They run discover to reconstruct pool.json,
# then verify the recovered config actually unlocks the pool.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("discover lists labeled disks and prints write hint"):
    out = machine.succeed("braid discover 2>&1")
    assert "disk1" in out, "expected disk1 in discover output"
    assert "disk2" in out, "expected disk2 in discover output"
    assert "pass --write to save to /var/lib/braid/pool.json" in out, "expected write hint"

with subtest("discover without --write does not create pool.json"):
    machine.fail("test -f /var/lib/braid/pool.json")

with subtest("discover --write creates pool.json"):
    out = machine.succeed("braid discover --write 2>&1")
    assert "pool membership written to /var/lib/braid/pool.json" in out
    machine.succeed("test -f /var/lib/braid/pool.json")

with subtest("pool.json contains disk entries with by-id paths"):
    pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    assert "disk1" in pool_json, "expected disk1 in pool.json"
    assert "disk2" in pool_json, "expected disk2 in pool.json"
    assert "/dev/disk/by-id/" in pool_json, "expected by-id path in pool.json"

with subtest("recovered pool.json is usable — unlock succeeds"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("discover fails when pool.json already exists"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out

machine.shutdown()
