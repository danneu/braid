# Test: ups-lb-during-remove-missing
#
# Intent: verify that a forced shutdown driven by upsmon's critical-
# state SHUTDOWNCMD while the `maybe_restore_raid1` soft balance
# triggered by `braid remove-missing` is in flight is a recoverable
# state. After reboot, `braid recover` MUST detect the paused soft
# balance and resume it (M1 remediation in `cli/src/recover.rs`),
# leaving the pool with all chunks back to RAID1 -- not stuck with
# unprotected single-profile chunks.
#
# Why it exists: ADR 020's "mid-mutation power loss is a supported
# recovery case" guarantee covers the remove-missing path. The
# Pre-M11 audit identified this exact scenario as a gap (the existing
# `emit_paused_balance_warning` only printed an advisory; it did not
# actually drain the paused balance). M1 closed the gap by inserting
# a paused-balance resume between `save_membership` and
# `clear_journal` in `cmd_recover`. This VM test pins that fix in
# place -- if the resume regresses, the post-recover assertion that
# no `Data, single` chunks remain will fail.
#
# Scenario: Operator's pool was running degraded (a disk had failed
# and the operator was using the array degraded-mounted while
# waiting for a replacement). New writes during the degraded period
# created single-profile chunks. The operator added a replacement
# disk, then started `braid remove-missing` to drop the dead disk's
# metadata reference; the UPS LB fired during the post-removal soft
# balance that converts those single-profile chunks back to RAID1.
# The next morning the operator runs `braid recover`. The pool comes
# back fully RAID1 -- no manual `btrfs balance resume` required.
#
# Setup notes
# -----------
# The pool is built RAW (cryptsetup luksFormat + mkfs.btrfs +
# manual mount), mirroring tests/repro/degraded-soft-balance.py. We
# bypass `braid add` for the construction phase because braid add
# runs an unconditional post-add `pool_balance_raid1` that would
# convert the very single-profile chunks we need the soft balance to
# operate on. The pool.json that braid needs is seeded by hand. This
# is a deliberate test-only shortcut; the production flow always
# goes through `braid add`.

import json
import shlex
import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_format = (
    "--batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000"
)


def luks_uuid_for_name(name):
    return {
        "disk1": "11111111-1111-1111-1111-111111111111",
        "disk2": "22222222-2222-2222-2222-222222222222",
        "disk3": "33333333-3333-3333-3333-333333333333",
    }[name]


def luks_format_open(name):
    """Format and open a LUKS container at /dev/disk/by-id/virtio-NAME."""
    dev = f"/dev/disk/by-id/virtio-{name}"
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksFormat {luks_format} "
        f"--uuid {luks_uuid_for_name(name)} --label braid-{name} {dev}"
    )
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksOpen --key-file=- "
        f"{dev} braid-{name}"
    )


def luks_open(name):
    """Open an existing LUKS container at /dev/disk/by-id/virtio-NAME."""
    dev = f"/dev/disk/by-id/virtio-{name}"
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksOpen --key-file=- "
        f"{dev} braid-{name}"
    )


# --- Phase 1: build a 2-disk LUKS + btrfs RAID1 pool RAW. ---

def get_devid_for_mapper(mapper):
    """Look up the devid btrfs assigned to /dev/mapper/<mapper>."""
    show = machine.succeed("btrfs filesystem show /mnt/storage")
    for line in show.splitlines():
        # Lines look like: "\tdevid    1 size 6.00GiB used 1.00GiB path /dev/mapper/braid-disk1"
        if f"/dev/mapper/{mapper}" in line:
            tokens = line.split()
            # tokens[0] == "devid", tokens[1] == <number>
            assert tokens[0] == "devid", f"unexpected line: {line!r}"
            return int(tokens[1])
    raise AssertionError(
        f"could not find devid for mapper {mapper} in:\n{show}"
    )


with subtest("Build 2-disk RAID1 pool raw"):
    luks_format_open("disk1")
    luks_format_open("disk2")
    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 "
        "/dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed(
        "mount -o noatime,skip_balance,subvolid=5 "
        "/dev/mapper/braid-disk1 /mnt/storage"
    )
    # Capture the devids btrfs assigned. We need disk2's devid to
    # seed pool.json so braid remove-missing can resolve --missing-id
    # back to a membership name.
    disk1_devid = get_devid_for_mapper("braid-disk1")
    disk2_devid = get_devid_for_mapper("braid-disk2")
    print(f"disk1 devid: {disk1_devid}, disk2 devid: {disk2_devid}")

with subtest("Write baseline RAID1 payload"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/baseline bs=1M count=512 status=none"
    )
    machine.succeed("sync")
    baseline_sha = machine.succeed("sha256sum /mnt/storage/baseline").split()[0]
    print(f"baseline sha256: {baseline_sha}")

# --- Phase 2: kill disk2 -- close its LUKS mapper, remount degraded
# with only disk1 healthy. Single-profile writes are forced because
# RAID1 cannot allocate chunks against a single healthy disk. ---

with subtest("Simulate disk2 death and remount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose braid-disk2")
    machine.succeed(
        "mount -o noatime,skip_balance,subvolid=5,degraded "
        "/dev/mapper/braid-disk1 /mnt/storage"
    )
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== pool after degraded remount ===\n{fi_show}")
    assert "missing" in fi_show.lower(), (
        f"degraded mount did not show missing device:\n{fi_show}"
    )

with subtest("Write degraded-mode payload to create single-profile chunks"):
    # 3 GiB of single-profile writes. The post-remove-missing soft
    # balance has to read each single chunk + write two RAID1 mirrors,
    # which on tmpfs-backed virtual disks takes long enough that we
    # can interrupt it well before completion. We also wait below for
    # the balance to be observably early (>=70% remaining) before
    # triggering LB, so even fast tmpfs throughput cannot let the
    # balance finish before umount.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/degraded-write bs=1M count=3000 status=none"
    )
    machine.succeed("sync")
    degraded_sha = machine.succeed("sha256sum /mnt/storage/degraded-write").split()[0]
    print(f"degraded-write sha256: {degraded_sha}")

with subtest("Single-profile chunks exist before remove-missing"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== fi df after degraded writes ===\n{fi_df}")
    assert "Data, single" in fi_df, (
        f"expected single-profile data chunks after degraded write:\n{fi_df}"
    )


# --- Phase 3: add disk3 raw (no braid auto-balance). After this the
# pool has disk1 + disk3 active and disk2 missing -- the precondition
# for `braid remove-missing` to make sense. We deliberately use raw
# `btrfs device add` instead of `braid add` because braid add runs an
# unconditional post-add `pool_balance_raid1` that would convert the
# single chunks we just created -- which is exactly what we want
# `braid remove-missing`'s soft balance to do as the slow phase the
# test interrupts. ---

with subtest("Add disk3 as the replacement (raw, no auto-balance)"):
    luks_format_open("disk3")
    machine.succeed(
        "btrfs device add /dev/mapper/braid-disk3 /mnt/storage"
    )

# --- Phase 4: seed the pool.json that `braid remove-missing` needs.
# This is a test-only shortcut because we bypassed `braid add`. ---

with subtest("Seed pool.json with all three disks (incl. devids)"):
    # braid remove-missing resolves --missing-id back to a membership
    # name by matching `devid` on each disk entry
    # (cli/src/remove_missing.rs:30-58). Seed the captured devids for
    # disk1 + disk2 so that resolution succeeds. disk3's devid does
    # not need to be present; recover writes it from the live probe.
    pool_json = {
        "disks": {
            luks_uuid_for_name("disk1"): {
                "name": "disk1",
                "by_id": "/dev/disk/by-id/virtio-disk1",
                "devid": disk1_devid,
            },
            luks_uuid_for_name("disk2"): {
                "name": "disk2",
                "by_id": "/dev/disk/by-id/virtio-disk2",
                "devid": disk2_devid,
            },
            luks_uuid_for_name("disk3"): {
                "name": "disk3",
                "by_id": "/dev/disk/by-id/virtio-disk3",
            },
        }
    }
    machine.succeed(
        "cat > /var/lib/braid/pool.json << 'EOF'\n"
        f"{json.dumps(pool_json)}\nEOF"
    )

with subtest("Activate braid-online so its ExecStop fires on shutdown"):
    machine.succeed("systemctl start braid-online.service")


# --- Phase 5: kick off `braid remove-missing` and wait for the soft
# balance to be in flight. The fast metadata-only `btrfs device delete
# missing` runs first; the soft balance is what we want to interrupt. ---

def get_missing_devid():
    """Find disk2's devid via braid status --json."""
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, f"no missing devids in braid status:\n{raw}"
    return int(devids[0])


with subtest("Start remove-missing and wait for soft balance in flight"):
    # Sanity check: the missing devid braid sees matches what we
    # captured before killing disk2.
    missing_devid = get_missing_devid()
    assert missing_devid == disk2_devid, (
        f"missing devid {missing_devid} != captured disk2 devid "
        f"{disk2_devid}"
    )
    print(f"missing devid: {missing_devid}")

    machine.execute(
        f"(braid remove-missing --missing-id {missing_devid} --yes) "
        f"> /tmp/remove-missing.log 2>&1 &"
    )

    # Wait until the balance is observably running AND has at least
    # 70% of work remaining. Without the pct-left lower bound, we
    # might catch the balance at 95% complete and then it finishes
    # naturally during the ~1s shutdown window -- the M1 fix would
    # have nothing to do (Idle, not Paused). The plan's M5 explicitly
    # tests the M1 fix, so we need the balance to be far enough from
    # completion that umount cancels it mid-flight.
    import re
    PCT_RE = re.compile(r"(\d+)% left")

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
        "Never observed the soft balance running with >=70% of work "
        "remaining. The single-profile chunk count may have been too "
        "small for the balance to take measurable time, or "
        "remove-missing failed in preflight. Last balance status:\n"
        f"{last_status}\n"
        "remove-missing log:\n"
        + machine.execute("cat /tmp/remove-missing.log 2>&1")[1]
    )

# --- Phase 6: trigger UPS critical, wait for shutdown. ---

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

# --- Phase 7: reboot, run recover. ---

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
    assert '"RemoveMissing"' in journal_text, (
        f"journal is not OpKind::RemoveMissing as expected:\n{journal_text}"
    )

with subtest("braid recover completes cleanly"):
    # disk2 is genuinely gone (LUKS mapper closed, btrfs metadata
    # already removed by the in-flight remove-missing). Recovery
    # must succeed without --allow-degraded because the live pool is
    # disk1 + disk3 (= the post-remove-missing target), not degraded.
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
    # The M1 fix logs this when the post-mutation soft balance replays.
    # This message appears regardless of whether the umount paused or
    # cancelled the original balance.
    assert "replaying post-remove-missing RAID1 soft balance" in recover_out, (
        f"recover did not replay the post-remove-missing soft balance "
        f"-- the M1 remediation may have regressed.\n{recover_out}"
    )

# --- Phase 8: post-recover state assertions. ---

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

with subtest("No single-profile chunks remain (the M1 resume drained them)"):
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    print(f"=== final fi df ===\n{fi_df}")
    assert "Data, single" not in fi_df, (
        f"single-profile chunks still present -- M1 paused-balance resume "
        f"failed to drain them:\n{fi_df}"
    )
    assert "Metadata, single" not in fi_df, (
        f"single-profile metadata still present:\n{fi_df}"
    )

with subtest("Live pool has the post-remove-missing target membership"):
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== final fi show ===\n{fi_show}")
    assert "missing" not in fi_show.lower(), (
        f"missing device still present after recover:\n{fi_show}"
    )
    assert "Total devices 2" in fi_show, (
        f"post-recover topology should have 2 devices:\n{fi_show}"
    )
    for present in ["braid-disk1", "braid-disk3"]:
        assert present in fi_show, (
            f"{present} missing from live pool:\n{fi_show}"
        )

with subtest("pool.json reflects the post-remove-missing membership"):
    final_pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== final pool.json ===\n{final_pool_json}")
    assert '"disk1"' in final_pool_json
    assert '"disk3"' in final_pool_json
    assert '"disk2"' not in final_pool_json

with subtest("Journal cleared after recover"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("No paused balance left behind"):
    status_out = machine.execute("btrfs balance status /mnt/storage 2>&1")[1]
    print(f"=== final balance status ===\n{status_out}")
    assert "paused" not in status_out, (
        f"a paused balance still exists after recover -- the M1 resume "
        f"either did not run or did not complete:\n{status_out}"
    )

with subtest("Both payload checksums survived"):
    baseline_post = machine.succeed("sha256sum /mnt/storage/baseline").split()[0]
    assert baseline_post == baseline_sha, (
        f"baseline payload changed: pre={baseline_sha} post={baseline_post}"
    )
    degraded_post = machine.succeed("sha256sum /mnt/storage/degraded-write").split()[0]
    assert degraded_post == degraded_sha, (
        f"degraded-write payload changed: pre={degraded_sha} post={degraded_post}"
    )

with subtest("Subsequent lock/unlock cycle stays clean"):
    machine.succeed("braid lock")
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    cycle_fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "missing" not in cycle_fs_show.lower(), (
        f"missing re-appeared after lock/unlock cycle:\n{cycle_fs_show}"
    )
    assert "Total devices 2" in cycle_fs_show, (
        f"pool should still have 2 devices:\n{cycle_fs_show}"
    )

machine.shutdown()
