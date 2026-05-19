# Test: braid-lock-systemd-stop
#
# Intent:
#   braid-online.service ExecStop invokes `braid lock --systemd-stop` with a
#   bounded wait for /run/braid-pool.lock.
#
# Why it exists:
#   Shutdown must wait for ordinary in-flight braid work, but fail before
#   systemd SIGKILLs the stop job when the pool lock never releases.
#
# Scenario:
#   systemd stops braid-online while another process temporarily or permanently
#   holds the pool-operation lock.

import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Unlock pool and activate braid-online"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)

with subtest("ExecStop waits for temporary pool-lock holder"):
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 2' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")

with subtest("Re-unlock pool for deadline case"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)

with subtest("ExecStop reports deadline expiry when lock stays held"):
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        "sh -c 'touch /tmp/holder.ready; sleep 30' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    rc, out = machine.execute("systemctl stop braid-online.service 2>&1")
    machine.execute(f"kill {holder_pid} 2>/dev/null || true")
    assert rc == 0, "systemctl stop job should complete; out=" + out
    result = machine.succeed("systemctl show -P Result braid-online.service").strip()
    assert result == "exit-code", "expected ExecStop failure result, got " + result
    journal = machine.succeed("journalctl -u braid-online.service -n 80 --no-pager -o cat")
    assert "aborting --systemd-stop" in journal, journal
