# Test: scrub-alert
#
# Intent: Verify that a genuinely failed maintenance scrub raises braid's
#   user-facing alert end to end (onFailure -> scrub-failed flag -> immediate
#   beeper + alertCommand -> braid status names the cause -> monitor latches it
#   at Critical -> braid ack clears everything), while the silent paths stay
#   silent: btrfs exit 3 (corruption found, scrub completed) is a service
#   success that routes to the device-stats poll, exit 0 (clean scrub) never
#   beeps, exit 4 (the busy gate skipped this run) is a retryable success rather
#   than a failure, and a deliberate lock-time cancel of a REAL scrub resolves to
#   Result=success without firing onFailure.
#
# Why it exists: braid-scrub.service previously had no failure alerting. Wiring
#   onFailure is only safe because (1) a deliberate cancel writes a
#   cancel-request marker so scrub-resume-or-start exits 0 even though btrfs
#   exits 1, and (2) SuccessExitStatus=3 keeps scrub-found corruption off the
#   onFailure path. Both are behavioral and untestable by unit tests alone.
#
# Scenario: Two nodes, each a 2-disk RAID1 pool with monitor enabled.
#   fail:   exit-code-parameterized scrub. Exit 1 -> alert raised + cleared;
#           exit 3, exit 0 and exit 4 -> silent.
#   cancel: dm-delay-backed REAL scrub cancelled by `braid lock` -> silent,
#           Result=success. A real scrub is mandatory: the fake `sleep 300`
#           scrub is SIGTERM-clean and would not exercise the real
#           btrfs-exit-1-on-cancel + marker path.

import json
import shlex

start_all()

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

SERVICE = "braid-scrub.service"
SCRUB_EXIT_FILE = "/run/braid-test-scrub-exit"
SCRUB_FAILED_FLAG = "/var/lib/braid/scrub-failed"
LATCH = "/var/lib/braid/alert-latch.json"
ALERT_FIRED = "/root/alert-fired"

# dm-delay params for the cancel node's real scrub (mirrors scrub-lifecycle).
SCRUB_PAYLOAD_MIB = 32
SCRUB_READ_DELAY_MS = 500
SCRUB_DELAY_DISKS = ["disk1", "disk2"]


def show(node, unit, prop):
    return node.succeed("systemctl show {} -p {} --value".format(unit, prop)).strip()


def luks_uuid_for_name(name):
    return {
        "disk1": "11111111-1111-1111-1111-111111111111",
        "disk2": "22222222-2222-2222-2222-222222222222",
    }[name]


def unlock(node):
    node.succeed("printf '%s\\n' {} | braid unlock --passphrase-stdin".format(pq))


def status_json(node):
    return json.loads(node.succeed("braid status --json"))


def has_scrub_failed_cause(data):
    return any(c.get("type") == "scrub_failed" for c in data["alert_causes"])


def setup_resume_pool(node):
    for name in SCRUB_DELAY_DISKS:
        dm_delay_create(node, name)
        by_id = "/dev/disk/by-id/braid-test-{}-delay".format(name)
        node.succeed(
            "printf '%s' {} | cryptsetup luksFormat --batch-mode --key-file=- "
            "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
            "--uuid {} --label braid-{} {}".format(
                pq, luks_uuid_for_name(name), name, by_id
            )
        )
        node.succeed(
            "printf '%s' {} | cryptsetup open --key-file=- {} braid-{}".format(
                pq, by_id, name
            )
        )

    node.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 "
        "/dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    node.succeed("cryptsetup close braid-disk1")
    node.succeed("cryptsetup close braid-disk2")
    pool_json = {
        "disks": {
            luks_uuid_for_name(name): {
                "name": name,
                "by_id": "/dev/disk/by-id/braid-test-{}-delay".format(name),
            }
            for name in SCRUB_DELAY_DISKS
        }
    }
    node.succeed(
        "cat > /var/lib/braid/pool.json << 'EOF'\n{}\nEOF".format(json.dumps(pool_json))
    )


# ===========================================================================
# fail node: a genuine failure raises and clears the alert; clean and
# corruption exits stay silent.
# ===========================================================================

with subtest("fail: unlock the pool"):
    fail.wait_for_unit("multi-user.target", timeout=120)
    unlock(fail)
    fail.succeed("systemctl is-active braid-online.service")
    fail.succeed("mountpoint -q /mnt/storage")

with subtest("fail: scrub failure hook carries the root sandbox"):
    unit = fail.succeed("systemctl cat braid-scrub-failed.service")
    assert "ProtectSystem=strict" in unit, (
        "scrub failure hook must use ProtectSystem=strict:\n" + unit
    )
    assert "ReadWritePaths=/var/lib/braid" in unit, (
        "scrub failure hook must keep braid state writable:\n" + unit
    )
    assert "CapabilityBoundingSet=" in unit, (
        "scrub failure hook must drop all capabilities:\n" + unit
    )
    assert "RestrictAddressFamilies=AF_UNIX" in unit, (
        "scrub failure hook must restrict to AF_UNIX:\n" + unit
    )
    assert show(fail, "braid-scrub-failed.service", "NoNewPrivileges") == "yes"

with subtest("fail: a failed scrub (exit 1) leaves the unit failed"):
    fail.succeed("echo 1 > {}".format(SCRUB_EXIT_FILE))
    fail.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    # Type=simple + an instantly-exiting ExecStart: `systemctl start` returns as
    # soon as it forks, so the start itself may report the fast failure -- use
    # execute and wait for the failed transition instead.
    fail.execute("systemctl start {}".format(SERVICE))
    fail.wait_until_succeeds("systemctl is-failed {}".format(SERVICE), timeout=30)

with subtest("fail: onFailure fired -- flag set, beeper started, alertCommand ran"):
    fail.wait_until_succeeds("test -f {}".format(SCRUB_FAILED_FLAG), timeout=30)
    mode = fail.succeed("stat -c %a {}".format(SCRUB_FAILED_FLAG)).strip()
    assert mode == "600", "expected scrub-failed mode 600, got {}".format(mode)
    fail.wait_until_succeeds("systemctl is-active braid-alert.service", timeout=30)
    fail.wait_until_succeeds("test -f {}".format(ALERT_FIRED), timeout=30)

with subtest("fail: braid status names the ScrubFailed cause from the flag"):
    data = status_json(fail)
    assert data["alert_active"] is True, data
    assert has_scrub_failed_cause(data), data

with subtest("fail: monitor latches ScrubFailed at Critical (beeper, not advisory)"):
    fail.succeed("systemctl start braid-monitor.service")
    fail.wait_until_succeeds("grep -q scrub_failed {}".format(LATCH), timeout=30)
    fail.succeed("systemctl is-active braid-alert.service")
    # Load-bearing F1 assertion: a Critical scrub-failed latch routes to the
    # exit-1 beeper, NOT the exit-3 ENOSPC/Warning advisory.
    fail.fail("systemctl is-active braid-alert-advisory.service")

with subtest("fail: braid ack clears flag, latch, and beeper"):
    fail.succeed("braid ack")
    fail.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    fail.fail("systemctl is-active braid-alert.service")
    fail.fail("test -f {}".format(LATCH))
    data = status_json(fail)
    assert data["alert_active"] is False, data

with subtest("fail: exit 3 (corruption found, scrub completed) is a success, no alert"):
    fail.succeed("echo 3 > {}".format(SCRUB_EXIT_FILE))
    fail.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    fail.execute("systemctl start {}".format(SERVICE))
    # SuccessExitStatus=3 -> the unit never enters `failed`, so onFailure cannot
    # fire. Wait for the run to settle, then assert success + silence.
    fail.wait_until_succeeds(
        'test "$(systemctl show {} -p Result --value)" = success'.format(SERVICE),
        timeout=30,
    )
    fail.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    fail.fail("systemctl is-active braid-alert.service")

with subtest("fail: exit 0 (clean scrub) stays silent -- the headline promise"):
    fail.succeed("echo 0 > {}".format(SCRUB_EXIT_FILE))
    fail.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    fail.execute("systemctl start {}".format(SERVICE))
    fail.wait_until_succeeds(
        'test "$(systemctl show {} -p Result --value)" = success'.format(SERVICE),
        timeout=30,
    )
    fail.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    fail.fail("systemctl is-active braid-alert.service")
    fail.fail("test -f {}".format(ALERT_FIRED))

with subtest("fail: exit 4 (busy skip) is a success, no alert"):
    # The gate's skip code. SuccessExitStatus covers it, so onFailure cannot
    # fire -- but RestartForceExitStatus schedules a retry, so this subtest runs
    # last on this node and stops the unit rather than leaving it in
    # auto-restart under the other subtests.
    fail.succeed("echo 4 > {}".format(SCRUB_EXIT_FILE))
    fail.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    fail.execute("systemctl start {}".format(SERVICE))
    fail.wait_until_succeeds(
        'test "$(systemctl show {} -p ExecMainStatus --value)" = 4'.format(SERVICE),
        timeout=30,
    )
    assert show(fail, SERVICE, "Result") == "success", (
        "a busy skip must be a service success"
    )
    fail.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    fail.fail("systemctl is-active braid-alert.service")
    fail.fail("test -f {}".format(ALERT_FIRED))
    fail.succeed("systemctl stop {}".format(SERVICE))

fail.shutdown()

# ===========================================================================
# cancel node: a lock mid-REAL-scrub does NOT alert.
# ===========================================================================

with subtest("cancel: prepare dm-delay pool, unlock, write payload, arm delay"):
    cancel.wait_for_unit("multi-user.target", timeout=120)
    setup_resume_pool(cancel)
    unlock(cancel)
    cancel.succeed(
        "dd if=/dev/urandom of=/mnt/storage/scrub-payload bs=1M count={} status=none".format(
            SCRUB_PAYLOAD_MIB
        )
    )
    cancel.succeed("sync")
    dm_delay_activate(cancel, SCRUB_DELAY_DISKS, read_delay_ms=SCRUB_READ_DELAY_MS)

with subtest("cancel: start the real scrub and wait until it is running"):
    cancel.succeed("rm -f {} {}".format(SCRUB_FAILED_FLAG, ALERT_FIRED))
    cancel.succeed("systemctl start {}".format(SERVICE))
    cancel.succeed(
        "for i in $(seq 1 400); do "
        'out="$(btrfs scrub status --raw /mnt/storage 2>&1 || true)"; '
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

with subtest("cancel: braid lock cancels with Result=success (marker discrimination)"):
    cancel.succeed("braid lock")
    # The whole point: ExecStop wrote the cancel-request marker before the
    # cancel ioctl, so scrub-resume-or-start read it and exited 0 even though
    # btrfs exited 1. A real cancel is therefore success, not failed.
    result = show(cancel, SERVICE, "Result")
    assert result == "success", "real cancel must be success, got Result={}".format(
        result
    )

with subtest("cancel: the deliberate cancel raised no alert"):
    cancel.fail("systemctl is-active braid-alert.service")
    cancel.fail("test -f {}".format(SCRUB_FAILED_FLAG))
    cancel.fail("test -f {}".format(ALERT_FIRED))

cancel.shutdown()
