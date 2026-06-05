# Test: ups-lb-during-balanced-add
#
# Intent: verify that a forced shutdown driven by upsmon's critical-
# state SHUTDOWNCMD while the post-add `pool_balance_raid1`
# conversion is in flight either recovers on the idle/no-paused path
# or fails closed when btrfs persists the balance as paused. After
# reboot, `braid recover` MUST NOT automate crash-paused owed RAID1
# replay; it preserves the journal for manual reconciliation instead.
#
# Why it exists: ADR 020's "mid-mutation power loss is a supported
# recovery case" guarantee covers the balanced-add path. The
# Pre-M11 audit identified this exact scenario as a gap. Later VM
# evidence showed that automatic replay after a crash-paused owed
# RAID1 balance can underflow btrfs block-group accounting. This test
# pins both supported outcomes: idle/no-paused recovery still replays
# the soft balance and clears the journal, while a persisted paused
# balance fails closed before replay.
#
# Scenario: Operator's pool was 1-disk single-profile with data.
# They added a second disk via `braid add` to gain
# RAID1 redundancy. The UPS LB fired during the post-add balance
# that converts single-profile chunks to RAID1. The next morning
# they run `braid recover`. If btrfs has no paused balance, recover
# finishes the soft RAID1 replay; if btrfs persisted a paused balance,
# recover leaves `pending-op.json` in place and requires manual
# inspection before the recovery state is cleared.

import re
import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
DELAYED_DISKS = ["disk2"]


def disk_path(key):
    if key in DELAYED_DISKS:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


def add_cmd_bg(key):
    # Background the entire (passphrase | braid add) pipeline so
    # machine.execute returns immediately. The subshell ensures the
    # & applies to the whole pipeline rather than just the tail.
    return (
        f"(printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} "
        f"--passphrase-stdin --yes) > /tmp/add.log 2>&1 &"
    )


# --- Phase 1: build a 1-disk single-profile pool, write data. ---

with subtest("Build 1-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload as single-profile chunks"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=100 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

with subtest("Pre-add: chunks are single-profile"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== fi df pre-add ===\n{fi_df}")
    assert "Data, single" in fi_df, (
        f"expected single-profile data on a 1-disk pool:\n{fi_df}"
    )
    assert "Data, RAID1" not in fi_df, (
        f"unexpected RAID1 data on a 1-disk pool:\n{fi_df}"
    )

# --- Phase 2: kick off `braid add disk2` and wait for the post-add
# RAID1 conversion to be observably early in flight. We require the
# balance to be at >=70% remaining so it cannot finish naturally
# during the ~1s shutdown window. ---

PCT_RE = re.compile(r"(\d+)% left")

with subtest("Start braid add and wait for post-add balance in flight"):
    dm_delay_create(machine, "disk2")
    dm_delay_activate(machine, "disk2", write_delay_ms=500)
    machine.execute(add_cmd_bg("disk2"))

    saw_balance_with_room = False
    last_status = ""
    for _ in range(800):  # 40s budget
        status_ret = machine.execute("btrfs balance status /mnt/storage 2>&1")
        last_status = status_ret[1]
        if "is running" in last_status:
            m = PCT_RE.search(last_status)
            if m and int(m.group(1)) >= 70:
                saw_balance_with_room = True
                print(f"balance status: {last_status.strip()}")
                break
        time.sleep(0.05)

    assert saw_balance_with_room, (
        "Never observed the post-add balance running with >=70% of work "
        "remaining. The single-profile chunk count may have been too "
        "small for the balance to take measurable time, or braid add "
        "failed in preflight. Last balance status:\n"
        f"{last_status}\n"
        "add log:\n"
        + machine.execute("cat /tmp/add.log 2>&1")[1]
    )

# --- Phase 3: trigger UPS critical, wait for shutdown. ---

with subtest("Drive UPS critical: upsrw ups.status = OB LB"):
    machine.succeed(
        "upsrw -s 'ups.status=OB LB' "
        "-u testops -p testpass ups@localhost"
    )

with subtest("Host shuts down in response to upsmon SHUTDOWNCMD"):
    try:
        machine.wait_for_shutdown()
    except Exception as e:
        _rc, upsmon_log = machine.execute(
            "journalctl -u upsmon.service --no-pager -n 100"
        )
        raise AssertionError(
            f"host did not shut down after OB+LB. upsmon journal:\n{upsmon_log}"
        ) from e

# --- Phase 4: reboot, run recover. ---

machine.start()
machine.wait_for_unit("multi-user.target", timeout=120)
for name in DELAYED_DISKS:
    dm_delay_create(machine, name)

with subtest("Previous boot's braid-online.service stopped cleanly"):
    svc_log = machine.succeed(
        "journalctl -b -1 -u braid-online.service --no-pager"
    )
    assert "Stopped Braid storage pool online" in svc_log, (
        f"ExecStop did not complete during upsmon-triggered shutdown.\n"
        f"Journal:\n{svc_log}"
    )

with subtest("Pending-op journal survived the forced shutdown"):
    machine.succeed("test -f /var/lib/braid/pending-op.json")
    journal_text = machine.succeed("cat /var/lib/braid/pending-op.json")
    print(f"=== surviving journal ===\n{journal_text}")
    assert '"Add"' in journal_text, (
        f"journal is not OpKind::Add as expected:\n{journal_text}"
    )

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, (
        f"unlock should refuse with journal, got exit 0:\n{output}"
    )
    assert "interrupted operation" in output, (
        f"unlock did not emit the journal-detected error:\n{output}"
    )

recover_failed_closed = False
recover_out = ""

with subtest("braid recover handles the post-add balance state"):
    recover_exit, recover_out = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin 2>&1"
    )
    print(f"=== braid recover (exit {recover_exit}) ===\n{recover_out}")
    assert "panicked at" not in recover_out, (
        f"braid recover panicked:\n{recover_out}"
    )

    if recover_exit == 0:
        # Idle/no-paused path: recover should replay the idempotent soft balance
        # and clear recovery mode.
        assert "replaying post-add RAID1 soft balance" in recover_out, (
            f"recover did not replay the post-add soft balance:\n{recover_out}"
        )
        assert "balance resume" not in recover_out, (
            f"recover must not resume balances automatically:\n{recover_out}"
        )
    else:
        recover_failed_closed = True
        assert "preserving pending-op.json" in recover_out, (
            f"fail-closed recover did not preserve the journal:\n{recover_out}"
        )
        assert (
            "paused btrfs balance" in recover_out
            or "running btrfs balance" in recover_out
            or "could not determine btrfs balance state" in recover_out
        ), f"recover did not name the balance-state refusal:\n{recover_out}"
        assert "balance resume" not in recover_out, (
            f"recover must not resume a crash-paused balance:\n{recover_out}"
        )
        assert "balance cancel" not in recover_out, (
            f"recover must not cancel a crash-paused balance:\n{recover_out}"
        )
        assert "replaying post-add RAID1 soft balance" not in recover_out, (
            f"recover must fail before soft RAID1 replay:\n{recover_out}"
        )

# --- Phase 5: post-recover state assertions. ---

with subtest("Pool is mounted after recover"):
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("btrfs reports zero device errors"):
    stats = machine.succeed("btrfs device stats /mnt/storage")
    for line in stats.splitlines():
        parts = line.rsplit(maxsplit=1)
        if len(parts) == 2:
            assert parts[1] == "0", (
                f"btrfs reports non-zero stat after recover: {line!r}"
            )

with subtest("Single-profile chunk state matches recovery outcome"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== final fi df ===\n{fi_df}")
    if recover_failed_closed:
        assert "Data, single" in fi_df, (
            f"fail-closed recovery should leave Data,single visible:\n{fi_df}"
        )
    else:
        assert "Data, single" not in fi_df, (
            f"single-profile chunks still present after idle-path replay:\n{fi_df}"
        )

with subtest("Live pool has the post-add target membership"):
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== final fi show ===\n{fi_show}")
    assert "MISSING" not in fi_show, (
        f"phantom MISSING entry after recover:\n{fi_show}"
    )
    assert "Total devices 2" in fi_show, (
        f"post-recover topology should have 2 devices:\n{fi_show}"
    )
    for present in ["braid-disk1", "braid-disk2"]:
        assert present in fi_show, (
            f"{present} missing from live pool:\n{fi_show}"
        )

with subtest("pool.json reflects the post-add target membership"):
    final_pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== final pool.json ===\n{final_pool_json}")
    assert '"disk1"' in final_pool_json
    assert '"disk2"' in final_pool_json

with subtest("Journal state matches recovery outcome"):
    if recover_failed_closed:
        machine.succeed("test -f /var/lib/braid/pending-op.json")
    else:
        machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("Balance state matches recovery outcome"):
    status_out = machine.execute("btrfs balance status /mnt/storage 2>&1")[1]
    print(f"=== final balance status ===\n{status_out}")
    if recover_failed_closed:
        assert "No balance found" not in status_out, (
            f"fail-closed recovery should leave balance work visible:\n{status_out}"
        )
    else:
        assert "paused" not in status_out, (
            f"a paused balance still exists after recover:\n{status_out}"
        )

with subtest("Payload checksum survived"):
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_sha}"
    )

with subtest("Subsequent command behavior matches recovery outcome"):
    if recover_failed_closed:
        unlock_exit, unlock_out = machine.execute(
            f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
        )
        assert unlock_exit != 0, (
            f"unlock should refuse while pending-op.json is preserved:\n{unlock_out}"
        )
        assert "interrupted operation" in unlock_out, (
            f"unlock did not report recovery mode:\n{unlock_out}"
        )
    else:
        machine.succeed("braid lock")
        machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
        machine.succeed("mountpoint -q /mnt/storage")
        cycle_fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
        assert "MISSING" not in cycle_fs_show, (
            f"MISSING re-appeared after lock/unlock cycle:\n{cycle_fs_show}"
        )
        assert "Total devices 2" in cycle_fs_show, (
            f"pool should still have 2 devices:\n{cycle_fs_show}"
        )

machine.shutdown()
