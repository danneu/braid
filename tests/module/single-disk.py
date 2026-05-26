start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

# Intent: `braid discover` (bare and --write) emits the membership rows on
#   stdout (the pipeable data product) while the "pass --write" hint, the
#   "pool membership written" confirmation, warnings, and errors stay on stderr.
# Why it exists: the rows were printed to stderr, so `braid discover | grep <disk>`
#   yielded nothing and the read-only product was not pipeable. No test pinned the
#   stream, so a regression could silently move it back.
# Scenario: operator rebuilding a lost pool.json pipes `braid discover` into a
#   filter to confirm a specific drive was found before writing.
with subtest("braid discover routes membership rows to stdout, prose to stderr"):
    machine.succeed("braid discover >/tmp/d.out 2>/tmp/d.err")
    out, err = machine.succeed("cat /tmp/d.out"), machine.succeed("cat /tmp/d.err")
    assert "= /dev/disk/by-id/" in out, f"row not on stdout: {out!r}"
    assert "pass --write to save" in err, f"hint not on stderr: {err!r}"
    assert "pass --write to save" not in out, f"hint leaked to stdout: {out!r}"
    machine.succeed("test ! -e /var/lib/braid/pool.json")

    machine.succeed("braid discover --write >/tmp/dw.out 2>/tmp/dw.err")
    outw, errw = machine.succeed("cat /tmp/dw.out"), machine.succeed("cat /tmp/dw.err")
    assert "= /dev/disk/by-id/" in outw, f"row not on stdout: {outw!r}"
    assert "pool membership written" in errw, f"confirmation not on stderr: {errw!r}"
    assert "pool membership written" not in outw, f"confirmation leaked to stdout: {outw!r}"
    machine.succeed("test -e /var/lib/braid/pool.json")

with subtest("braid unlock opens LUKS and mounts pool"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("btrfs single-disk pool has correct profile"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    print(f"Pool:\n{fi_show}")
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk missing from pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, single" in df_output, f"Expected single profile:\n{df_output}"

with subtest("Runtime config file is generated"):
    import json
    config_raw = machine.succeed("cat /etc/braid/config.json")
    config = json.loads(config_raw)
    assert config["mount_point"] == "/mnt/storage", f"Expected /mnt/storage, got {config['mount_point']}"

with subtest("Unified CLI is on PATH"):
    machine.succeed("which braid")

with subtest("Write and read round-trip"):
    machine.succeed("echo 'hello braid' > /mnt/storage/test.txt")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "hello braid", f"Expected 'hello braid', got '{content}'"

machine.shutdown()
