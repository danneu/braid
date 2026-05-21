# Test: mark-online-skips-start-while-deactivating
#
# Intent:
#   A mount-producing mutator must not start braid-online.service when its
#   lock-held snapshot observed the unit deactivating.
#
# Why it exists:
#   Starting the lifecycle unit while a stop job is already in progress can
#   queue behind that stop and recreate the systemd lifecycle deadlock.
#
# Scenario:
#   braid-online is held in `deactivating` by a test drop-in while `braid add`
#   succeeds. The pool remains mounted, but braid-online finishes inactive.
#   Covered mutators: `braid add` (LUKS-format + mount path) and
#   `braid recover` (already-mounted skip path: `plan_open_pool` returns
#   `None`, so `InitialOpenPool` is not pushed).

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)

with subtest("Install slow ExecStop drop-in"):
    machine.succeed("mkdir -p /run/systemd/system/braid-online.service.d")
    machine.succeed(
        "cat > /run/systemd/system/braid-online.service.d/99-delay-stop.conf <<'EOF'\n"
        "[Service]\n"
        "ExecStop=\n"
        "ExecStop=/run/current-system/sw/bin/sleep 10\n"
        "EOF\n"
        "systemctl daemon-reload"
    )

with subtest("Unlock pool"):
    machine.succeed(f"printf %s\\\\n {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.wait_until_succeeds("systemctl is-active --quiet braid-online.service", timeout=30)

with subtest("Hold braid-online in deactivating"):
    stop_pid = machine.succeed("nohup systemctl stop braid-online.service >/tmp/stop.log 2>&1 & echo $!").strip()
    machine.wait_until_succeeds(
        "test \"$(systemctl show -P ActiveState braid-online.service)\" = deactivating",
        timeout=10,
    )

with subtest("braid add succeeds without re-starting braid-online"):
    machine.succeed(
        f"printf %s\\\\n {pq} | "
        "braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes"
    )
    machine.wait_until_fails(f"kill -0 {stop_pid} 2>/dev/null", timeout=30)
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")

with subtest("braid recover succeeds without re-starting braid-online"):
    # Re-activate braid-online so we can put it back into deactivating.
    machine.succeed("systemctl start braid-online.service")
    machine.wait_until_succeeds(
        "systemctl is-active --quiet braid-online.service", timeout=30
    )

    # Inject a live-pool reconcile journal: PostAddBalanceRaid1 with
    # matching membership. Mirrors tests/module/systemd-lifecycle.py
    # subtest 8.
    pool_json_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool_membership = json.loads(pool_json_raw)
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "phase": "PostAddBalanceRaid1",
            "targets": {},
        },
        "pre_membership": pool_membership,
        "target_membership": pool_membership,
    }
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )

    # Hold braid-online in deactivating (slow ExecStop drop-in is still
    # installed from the first subtest).
    stop_pid = machine.succeed(
        "nohup systemctl stop braid-online.service "
        ">/tmp/recover-stop.log 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds(
        "test \"$(systemctl show -P ActiveState braid-online.service)\" "
        "= deactivating",
        timeout=10,
    )

    # Recover with snapshot=deactivating must succeed and must NOT
    # queue a systemctl start that fires after the stop drains.
    machine.succeed(
        f"printf %s\\\\n {pq} | braid recover --passphrase-stdin"
    )
    machine.wait_until_fails(f"kill -0 {stop_pid} 2>/dev/null", timeout=30)
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active --quiet braid-online.service")
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("Cleanup"):
    machine.succeed("rm -f /run/systemd/system/braid-online.service.d/99-delay-stop.conf")
    machine.succeed("systemctl daemon-reload")
    machine.succeed("braid lock")
