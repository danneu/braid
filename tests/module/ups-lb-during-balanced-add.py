# Test: ups-lb-during-balanced-add
#
# Intent: verify that a forced shutdown driven by upsmon's critical-
# state SHUTDOWNCMD while the post-add `pool_balance_raid1`
# conversion is in flight is a recoverable state. After reboot,
# `braid recover` MUST drain the in-flight balance and ensure all
# data ends up RAID1 -- not stuck with a mix of single-profile and
# RAID1 chunks, which would leave the new data unprotected.
#
# Why it exists: ADR 020's "mid-mutation power loss is a supported
# recovery case" guarantee covers the balanced-add path. The
# Pre-M11 audit identified this exact scenario as a gap (the existing
# `emit_paused_balance_warning` only printed an advisory). M1 closed
# the gap by inserting a paused-balance resume + per-op soft balance
# replay between `save_membership` and `clear_journal` in
# `cmd_recover`. This VM test pins that fix for the Add path -- if
# the replay regresses, the post-recover assertion that no `Data,
# single` chunks remain will fail.
#
# Scenario: Operator's pool was 1-disk single-profile with several
# GiB of data. They added a second disk via `braid add` to gain
# RAID1 redundancy. The UPS LB fired during the post-add balance
# that converts single-profile chunks to RAID1. The next morning
# they run `braid recover`. The pool comes back fully RAID1 -- no
# manual `btrfs balance start` required.

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


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def add_cmd_bg(key):
    # Background the entire (passphrase | braid add) pipeline so
    # machine.execute returns immediately. The subshell ensures the
    # & applies to the whole pipeline rather than just the tail.
    return (
        f"(printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} "
        f"--passphrase-stdin --yes) > /tmp/add.log 2>&1 &"
    )


# --- Phase 1: build a 1-disk single-profile pool, write data. ---

with subtest("Build 1-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload as single-profile chunks"):
    # 3 GiB on a 1-disk pool is single-profile by construction (no
    # second disk for RAID1 mirroring). The post-add balance will
    # convert these single chunks to RAID1, which on tmpfs-backed
    # virtual disks takes ~3s -- wider than the ~1s shutdown window.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
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

with subtest("braid recover completes cleanly"):
    recover_exit, recover_out = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin 2>&1"
    )
    print(f"=== braid recover (exit {recover_exit}) ===\n{recover_out}")
    assert recover_exit == 0, (
        f"braid recover failed (exit {recover_exit}):\n{recover_out}"
    )
    assert "panicked at" not in recover_out, (
        f"braid recover panicked:\n{recover_out}"
    )
    # The M1 fix logs this when the post-Add soft balance replays.
    assert "replaying post-add RAID1 soft balance" in recover_out, (
        f"recover did not replay the post-add soft balance -- the M1 "
        f"remediation may have regressed.\n{recover_out}"
    )
    # Soft pin for the paused-balance resume rows. Whether btrfs persists
    # a paused balance after the LB shutdown depends on timing -- if the
    # balance completed (or failed to write a clean paused state) before
    # the kernel umount, recover sees an idle balance and only the soft
    # balance replay below fires. If both rows do appear, they must be
    # ordered correctly per Principle 13.
    resume_wait = "[wait] pool: resuming paused balance left by interrupted add..."
    resume_ok = "[ok]   pool: balance resume complete"
    if resume_ok in recover_out:
        assert resume_wait in recover_out, (
            f"resume ok appears without preceding wait:\n{recover_out}"
        )
        assert recover_out.find(resume_wait) < recover_out.find(resume_ok), (
            f"paused-balance wait must precede ok:\n{recover_out}"
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

with subtest("No single-profile chunks remain (the M1 replay drained them)"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== final fi df ===\n{fi_df}")
    assert "Data, single" not in fi_df, (
        f"single-profile chunks still present -- M1 soft-balance replay "
        f"failed to drain them:\n{fi_df}"
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

with subtest("Journal cleared after recover"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("No paused balance left behind"):
    status_out = machine.execute("btrfs balance status /mnt/storage 2>&1")[1]
    print(f"=== final balance status ===\n{status_out}")
    assert "paused" not in status_out, (
        f"a paused balance still exists after recover:\n{status_out}"
    )

with subtest("Payload checksum survived"):
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_sha}"
    )

with subtest("Subsequent lock/unlock cycle stays clean"):
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
