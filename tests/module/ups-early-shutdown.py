# Test: ups-early-shutdown
#
# Intent: verify that braid-ups-guard.service, as shipped, rides through an
# outage shorter than braid.ups.shutdownAfter, and forces a clean shutdown
# through `upsmon -c fsd` once an outage outlasts it -- on the outage's
# original deadline even when the guard restarts partway through -- with
# braid-online's ExecStop and ups-killpower running on the way down.
#
# Why it exists: firmware LB is the latest possible moment to shut down, and
# the `battery.*.low` overrides braid's docs once suggested never took
# effect without `ignorelb`. The guard is the early-shutdown path
# (plans/impl/2026-10-22-1122-ups-on-battery-shutdown-policy.md, PO2-PO4).
# Each layer -- the unprivileged user, the sandbox's loopback-only network,
# the signal to upsmon, the preserved clock, POWERDOWNFLAG -- could be right
# alone and still fail together.
#
# Scenario: a home NAS on a small UPS. A 10-second flicker passes with no
# shutdown. Later a long outage starts; a `nixos-rebuild switch` restarts the
# guard partway through; the NAS still shuts down when the first outage
# timer runs out, with the battery to spare, and the UPS is told to cut and
# later restore power.

import shlex
import time

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
clock = "/run/braid-ups-guard/clock"
# Matches modules/braid ups.shutdownAfter in ups-early-shutdown.nix.
shutdown_after = 90


def set_status(status: str) -> None:
    machine.succeed(
        f"upsrw -s {shlex.quote('ups.status=' + status)} "
        "-u testops -p testpass ups@localhost"
    )


start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)
machine.wait_for_unit("braid-ups-guard.service", timeout=60)
machine.wait_until_succeeds("upsc ups@localhost ups.status | grep -qx OL", timeout=60)

with subtest("Guard runs as the upsmon user with no capabilities"):
    pid = machine.succeed("systemctl show -P MainPID braid-ups-guard.service").strip()
    user = machine.succeed(f"ps -o user= -p {pid}").strip()
    assert user == "nutmon", f"guard runs as {user!r}, expected nutmon"
    caps = machine.succeed(f"grep -E '^Cap(Eff|Prm):' /proc/{pid}/status")
    assert all(line.split()[1] == "0000000000000000" for line in caps.splitlines()), (
        f"guard holds capabilities:\n{caps}"
    )

with subtest("Unlock pool and seed canary file"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("echo 'early-shutdown-canary' > /mnt/storage/canary.txt")
    machine.succeed("sync")

with subtest("A short outage starts and then stops the clock, with no shutdown"):
    set_status("OB")
    short_start = time.monotonic()
    machine.wait_until_succeeds(f"test -s {clock}", timeout=30)
    time.sleep(10)
    set_status("OL")
    machine.wait_until_succeeds(f"! test -e {clock}", timeout=30)
    # Stay up past the point where the short outage's timer would have run
    # out, so a clock that kept running would have fired by now.
    remaining = shutdown_after + 15 - (time.monotonic() - short_start)
    if remaining > 0:
        time.sleep(remaining)
    machine.succeed("mountpoint -q /mnt/storage")
    guard_log = machine.succeed("journalctl -u braid-ups-guard.service --no-pager")
    assert "forcing shutdown" not in guard_log, (
        f"guard fired on a short outage:\n{guard_log}"
    )

with subtest("A long outage keeps its deadline across a guard restart"):
    set_status("OB")
    machine.wait_until_succeeds(f"test -s {clock}", timeout=30)
    long_start = time.monotonic()
    started_at = machine.succeed(f"cat {clock}").strip()
    time.sleep(shutdown_after * 2 // 3)
    machine.succeed("systemctl restart braid-ups-guard.service")
    restarted = time.monotonic()
    assert machine.succeed(f"cat {clock}").strip() == started_at, (
        "the guard restart changed the on-battery clock"
    )
    try:
        machine.wait_for_shutdown()
    except Exception as e:
        _rc, guard_log = machine.execute(
            "journalctl -u braid-ups-guard.service -u upsmon.service --no-pager -n 100"
        )
        raise AssertionError(
            f"host did not shut down after the timer. Journal:\n{guard_log}"
        ) from e
    since_restart = time.monotonic() - restarted
    # Original deadline: about shutdown_after / 3 after the restart, plus a
    # poll and the shutdown itself. A clock reset by the restart would not
    # even start the shutdown until shutdown_after after it.
    assert since_restart < shutdown_after - 10, (
        f"shutdown came {since_restart:.0f}s after the guard restart and "
        f"{time.monotonic() - long_start:.0f}s into the outage; the restart "
        "reset the clock"
    )

machine.start()
machine.wait_for_unit("multi-user.target")

with subtest("Previous boot: guard trigger, braid-online ExecStop, killpower"):
    guard_log = machine.succeed("journalctl -b -1 -u braid-ups-guard.service --no-pager")
    assert "forcing shutdown through upsmon: on battery for" in guard_log, (
        f"guard trigger line missing. Journal:\n{guard_log}"
    )
    svc_log = machine.succeed("journalctl -b -1 -u braid-online.service --no-pager")
    assert "Stopped Braid storage pool online" in svc_log, (
        f"ExecStop did not complete during the forced shutdown. Journal:\n{svc_log}"
    )
    assert "timed out" not in svc_log.lower(), (
        f"braid-online.service was killed by timeout. Journal:\n{svc_log}"
    )
    # Same proof as ups-lb-clean-shutdown: upsmon wrote POWERDOWNFLAG, so
    # ups-killpower ran the driver's shutdown. dummy-ups cannot cut power;
    # the hardware half is FU2 in the plan.
    killpower_log = machine.succeed(
        "journalctl -b -1 -u ups-killpower.service --no-pager"
    )
    assert "Starting UPS Kill Power" in killpower_log, (
        f"ups-killpower did not run. Journal:\n{killpower_log}"
    )
    assert "shutdown not supported" in killpower_log, (
        f"upsdrvctl shutdown did not reach the driver. Journal:\n{killpower_log}"
    )

with subtest("The guard starts the new boot with no clock"):
    machine.wait_for_unit("braid-ups-guard.service", timeout=60)
    machine.succeed(f"! test -e {clock}")

with subtest("Canary file intact on remount"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    content = machine.succeed("cat /mnt/storage/canary.txt").strip()
    assert content == "early-shutdown-canary", f"canary lost: {content!r}"
    machine.succeed("braid lock")

machine.shutdown()
