# Test: ups-lb-clean-shutdown
#
# Intent: verify that upsmon's critical-state trigger (OB + LB) runs
# `systemctl poweroff`, systemd unwinds braid-online.service's
# ExecStop = braid lock, and the pool unmounts cleanly with LUKS
# closed before the host powers off.
#
# Why it exists: this is v1 guarantee (1) -- orderly shutdown before
# battery exhaustion in ordinary mounted operation. Without this proof,
# every layer (SHUTDOWNCMD override, decision 018's ExecStop hook,
# systemd shutdown sequence) could be right in isolation while failing
# together under the real upsmon critical trigger.
#
# Scenario: real-world outage lasting long enough to drain the UPS past
# the low-battery threshold while the NAS is idle with the pool mounted.
# Simulated here via upsrw: switch ups.status to "OB LB" on a running
# pool, wait for upsmon's FINALDELAY-bounded SHUTDOWNCMD, then reboot
# the VM and assert data survived.

import shlex

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

machine.wait_for_unit("braid-ups-secrets.service", timeout=60)
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)

with subtest("Production upsmon user does not carry SET action"):
    # Per reference/nut/docs/man/upsd.users.txt:78, SET is only needed by
    # upsrw clients. The production upsmon credential must not carry it,
    # regardless of what the test-only testops user has.
    users_conf = machine.succeed("cat /run/nut/upsd.users")
    # Find the [ups] stanza (production) and check no actions = SET
    assert "[ups]" in users_conf, (
        f"production [ups] user missing from upsd.users; got:\n{users_conf}"
    )
    # Quick structural check: split stanzas and ensure the [ups] one has no
    # `actions = SET` line. The testops stanza is separate.
    stanzas: dict[str, list[str]] = {}
    current: str | None = None
    for line in users_conf.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            current = stripped[1:-1]
            stanzas[current] = []
        elif current is not None:
            stanzas[current].append(stripped)
    ups_stanza = stanzas.get("ups", [])
    assert not any("actions = SET" in line for line in ups_stanza), (
        f"production upsmon user must not carry SET; [ups] stanza lines:\n{ups_stanza}"
    )

with subtest("SHUTDOWNCMD override is systemctl poweroff"):
    upsmon_conf = machine.succeed("cat /run/nut/upsmon.conf")
    assert "systemctl poweroff" in upsmon_conf, (
        f"SHUTDOWNCMD must use systemctl poweroff; got:\n{upsmon_conf}"
    )

with subtest("Secret file is 0600 root:root outside the Nix store"):
    stat = machine.succeed("stat -c '%U:%G %a' /var/lib/braid/upsmon.pass").strip()
    assert stat == "root:root 600", f"expected root:root 600, got {stat}"

with subtest("Unlock pool and seed canary file"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("echo 'lb-shutdown-canary' > /mnt/storage/canary.txt")
    machine.succeed("sync")

with subtest("Drive UPS critical: upsrw ups.status = OB LB"):
    # upsmon declares critical when ST_ONBATT and ST_LOWBATT are both
    # set (reference/nut/clients/upsmon.c:1404). The `-s key=value` form
    # with quoted "OB LB" sends both tokens as one multi-flag status
    # string (reference/nut/clients/upsrw.c).
    machine.succeed(
        "upsrw -s 'ups.status=OB LB' "
        "-u testops -p testpass ups@localhost"
    )

with subtest("Host shuts down in response to upsmon SHUTDOWNCMD"):
    # FINALDELAY default is 5s before upsmon invokes SHUTDOWNCMD
    # (reference/nut/clients/upsmon.c:114,935). Plus systemd stop
    # sequence + braid-online ExecStop bounded by TimeoutStopSec=5min
    # (decision 018). Single-disk minimal pool should complete well
    # within the 180s wait budget below.
    try:
        machine.wait_for_shutdown()
    except Exception as e:
        # If wait_for_shutdown times out, emit the upsmon journal to
        # diagnose whether SHUTDOWNCMD fired.
        _rc, upsmon_log = machine.execute(
            "journalctl -u upsmon.service --no-pager -n 100"
        )
        raise AssertionError(
            f"host did not shut down after OB+LB. upsmon journal:\n{upsmon_log}"
        ) from e

# Second boot: verify data integrity and ExecStop journal evidence.
machine.start()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("Previous boot ran braid-online ExecStop to completion"):
    svc_log = machine.succeed(
        "journalctl -b -1 -u braid-online.service --no-pager"
    )
    assert "Stopped Braid storage pool online" in svc_log, (
        f"ExecStop did not complete during upsmon-triggered shutdown. Journal:\n{svc_log}"
    )
    assert "timed out" not in svc_log.lower(), (
        f"braid-online.service was killed by timeout during UPS shutdown. Journal:\n{svc_log}"
    )

with subtest("Canary file intact on remount + btrfs reports zero errors"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    content = machine.succeed("cat /mnt/storage/canary.txt").strip()
    assert content == "lb-shutdown-canary", (
        f"canary lost. expected 'lb-shutdown-canary', got '{content}'"
    )
    stats = machine.succeed("btrfs device stats /mnt/storage")
    for line in stats.splitlines():
        # Every stat line is "<path>.<metric>  <count>"; non-zero = error.
        parts = line.rsplit(maxsplit=1)
        if len(parts) == 2:
            assert parts[1] == "0", (
                f"btrfs reports non-zero stat after UPS shutdown: {line!r}"
            )

    machine.succeed("braid lock")

machine.shutdown()
