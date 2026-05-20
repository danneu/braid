# Test: post-mount-failure-marks-online
#
# Intent: a bootstrap `braid add` whose mount succeeds but whose
#   post-mount cleanup step fails must (a) leave braid-online.service
#   active, and (b) leave the pool recoverable via systemctl stop
#   braid-online.service even before pool.json is written.
# Why it exists: a previous bug left the pool mounted while
#   braid-online.service stayed inactive when cmd_add returned Err
#   post-mount; even after activating the service, ExecStop could
#   not unmount because pool.json was never written. Both halves of
#   the lifecycle hole must close (ADR 026).
# Scenario: single-disk fresh-pool bootstrap-add. Before `braid add`,
#   pre-create /var/lib/braid/acked-stats.json as a directory so the
#   post-mount alert::remove_acked_stats fails after pool_bootstrap_mount
#   has committed and BEFORE save_membership runs (so pool.json never
#   exists). Assert (1) the add exits non-zero, (2) the pool is mounted,
#   (3) braid-online.service is active, (4) pool.json does not exist,
#   (5) pending-op.json exists and is a bootstrap-add journal, (6)
#   `systemctl stop braid-online.service` tolerates missing pool.json,
#   unmounts /mnt/storage, and
#   closes the LUKS mapper.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = (
    "--luks-format-arg=--pbkdf "
    "--luks-format-arg=pbkdf2 "
    "--luks-format-arg=--pbkdf-force-iterations "
    "--luks-format-arg=1000"
)


with subtest("Bootstrap add fails after mount"):
    machine.succeed("mkdir -p /var/lib/braid/acked-stats.json")
    machine.succeed(
        f"ec=0; printf '%s\\n' {pq} | "
        f"braid add {luks_opts} disk1=/dev/disk/by-id/virtio-disk1 "
        f"--passphrase-stdin --yes >/tmp/add-out 2>&1 || ec=$?; "
        f"echo $ec >/tmp/add-exit"
    )
    exit_code = int(machine.succeed("cat /tmp/add-exit").strip())
    output = machine.succeed("cat /tmp/add-out")
    print(f"braid add output:\n{output}")

    assert exit_code != 0, "bootstrap add should fail after mount"
    assert "acked-stats cleanup failed at bootstrap" in output, output
    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active --quiet braid-online.service")
    machine.fail("test -e /var/lib/braid/pool.json")

with subtest("Bootstrap journal is available for lock cleanup"):
    raw = machine.succeed("cat /var/lib/braid/pending-op.json")
    journal = json.loads(raw)
    assert journal["op"]["op"] == "Add", journal
    assert journal["pre_membership"]["disks"] == {}, journal
    target_members = list(journal["target_membership"]["disks"].values())
    assert [member["name"] for member in target_members] == ["disk1"], journal

with subtest("ExecStop locks pool without pool.json"):
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("ls /dev/mapper/braid-* 2>/dev/null")
    machine.fail("systemctl is-active --quiet braid-online.service")

with subtest("Cleanup injected state"):
    machine.succeed(
        "rm -rf /var/lib/braid/acked-stats.json /var/lib/braid/pending-op.json"
    )
