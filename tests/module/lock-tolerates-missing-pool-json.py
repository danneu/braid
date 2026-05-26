# Test: lock-tolerates-missing-pool-json
#
# Intent:
#   `braid lock --dry-run`, `braid lock`, and `braid lock --systemd-stop`
#   tolerate missing /var/lib/braid/pool.json while handling open
#   /dev/mapper/braid-* mappers.
#
# Why it exists:
#   The documented `braid discover --write` recovery workflow may leave
#   pool.json missing while braid-online.service is still active. The
#   preview, operator-triggered, and shutdown-triggered cleanup paths must
#   stay aligned, and this catches edits that update only one dispatch arm.
#
# Scenario:
#   The pool unlocks normally; pool.json is moved aside; the operator previews
#   and then runs `braid lock` directly, then re-unlocks, moves pool.json
#   aside again, and lets braid-online.service ExecStop handle teardown.

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

with subtest("braid lock --dry-run previews cleanup without pool.json"):
    rc, _ = machine.execute(
        "braid lock --dry-run "
        ">/tmp/lock-missing-pool-json-dry-run.out "
        "2>/tmp/lock-missing-pool-json-dry-run.err"
    )
    stdout = machine.succeed("cat /tmp/lock-missing-pool-json-dry-run.out")
    stderr = machine.succeed("cat /tmp/lock-missing-pool-json-dry-run.err")

    assert rc == 0, "braid lock --dry-run should succeed without pool.json:\n" + stderr
    assert "pool.json unreadable" in stderr, "missing pool.json warning absent:\n" + stderr
    assert "close LUKS mapper" in stdout, "dry-run cleanup preview absent:\n" + stdout
    assert "orphaned mapper" in stdout, "dry-run orphan warning absent:\n" + stdout
    assert "nothing to do" not in stdout, "dry-run must not render a no-op:\n" + stdout
    machine.succeed("ls /dev/mapper/braid-* >/dev/null")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active --quiet braid-online.service")

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
