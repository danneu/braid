# Test: lock-tolerates-missing-pool-json
#
# Intent:
#   Both `braid lock` and `braid lock --systemd-stop` clean up open
#   /dev/mapper/braid-* mappers when /var/lib/braid/pool.json is missing.
#
# Why it exists:
#   The documented `braid discover --write` recovery workflow may leave
#   pool.json missing while braid-online.service is still active. Both the
#   operator-triggered and shutdown-triggered cleanup paths must complete,
#   and this catches edits that update only one dispatch arm.
#
# Scenario:
#   The pool unlocks normally; pool.json is moved aside; the operator first
#   runs `braid lock` directly, then re-unlocks, moves pool.json aside again,
#   and lets braid-online.service ExecStop handle teardown.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Unlock pool and remove pool.json"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)
    machine.succeed("mv /var/lib/braid/pool.json /var/lib/braid/pool.json.away")

with subtest("Plain braid lock closes mappers without pool.json"):
    rc, out = machine.execute("braid lock 2>&1")
    assert rc == 0, "braid lock should succeed without pool.json:\n" + out
    assert "pool.json unreadable" in out, "missing pool.json warning absent:\n" + out
    machine.fail("ls /dev/mapper/braid-* 2>/dev/null")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")

with subtest("Re-unlock pool and remove pool.json again"):
    machine.succeed("mv /var/lib/braid/pool.json.away /var/lib/braid/pool.json")
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)
    machine.succeed("mv /var/lib/braid/pool.json /var/lib/braid/pool.json.away")

with subtest("ExecStop closes mappers without pool.json"):
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("ls /dev/mapper/braid-* 2>/dev/null")
    machine.fail("systemctl is-active --quiet braid-online.service")
    machine.succeed(
        "journalctl -u braid-online.service --no-pager -o cat "
        "| grep -q 'pool.json unreadable'"
    )
