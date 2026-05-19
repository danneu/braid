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

with subtest("Cleanup"):
    machine.succeed("rm -f /run/systemd/system/braid-online.service.d/99-delay-stop.conf")
    machine.succeed("systemctl daemon-reload")
    machine.succeed("braid lock")
