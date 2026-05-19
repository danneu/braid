# Test: braid-lock-coordinator-race
#
# Intent:
#   A plain `braid lock` racing an external `systemctl stop
#   braid-online.service` must not deadlock until TimeoutStopSec.
#
# Why it exists:
#   Plain lock holds the stop coordinator while it synchronously stops the
#   lifecycle unit; the ExecStop reentry must poll for `done\n` instead of
#   blocking on the coordinator flock.
#
# Scenario:
#   Operator runs `braid lock` while another shell stops braid-online. Both
#   operations should finish with the pool offline and the unit inactive.

import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Unlock pool and activate braid-online"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)
    machine.wait_until_succeeds("systemctl is-active --quiet dummy-slow-consumer.service", timeout=30)

with subtest("Start plain braid lock and wait for coordinator"):
    lock_pid = machine.succeed("rm -f /tmp/lock.log; nohup braid lock >/tmp/lock.log 2>&1 & echo $!").strip()
    machine.wait_until_fails("flock -n /run/braid-stop-coordinator.lock true", timeout=10)

with subtest("External systemctl stop returns without deadline hang"):
    start = time.monotonic()
    machine.succeed("systemctl stop braid-online.service")
    elapsed = time.monotonic() - start
    assert elapsed < 20, f"systemctl stop took too long: {elapsed:.2f}s"

with subtest("Plain lock exits cleanly"):
    machine.wait_until_fails(f"kill -0 {lock_pid} 2>/dev/null", timeout=30)
    out = machine.succeed("cat /tmp/lock.log")
    assert "error:" not in out.lower(), out
    machine.fail("systemctl is-active --quiet dummy-slow-consumer.service")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")
