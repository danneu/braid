# Test: ups-lb-during-remove
#
# Intent: verify that a forced shutdown driven by upsmon's critical-
# state SHUTDOWNCMD while `braid remove` is migrating extents off the
# target device is a recoverable state. After reboot, `braid recover`
# returns either the pre-membership (3 devices; operator re-runs the
# remove) or the post-membership (2 devices; remove completed before
# umount). Either outcome is acceptable per the plan's matrix
# acceptance criteria; what is unacceptable is a stuck journal,
# orphaned LUKS mappers, or btrfs reporting device errors.
#
# Why it exists: ADR 020's "mid-mutation power loss is a supported
# recovery case" guarantee covers the remove path. The Pre-M11 audit
# (plans/impl/2026-04-21-forced-shutdown-recovery-proof.md, "Remove" section)
# concluded that the rollback case is well-handled by the existing
# recover code: live device count == pre count, recover writes pre
# membership, recovery_guidance prints "remove did not complete --
# re-run braid remove". This VM test exercises that path end-to-end
# under the actual systemd shutdown sequence the UPS LB trigger
# initiates.
#
# Scenario: Operator started `braid remove disk3` against a 3-disk
# RAID1 pool with data. Utility power dropped during
# the device-remove relocation; upsmon fired SHUTDOWNCMD; the host
# powered off mid-relocation. The next morning the operator boots the
# NAS and runs `braid recover`. The pool comes back; if the remove
# rolled back, the operator re-runs `braid remove disk3`; either way,
# no manual btrfs intervention is needed.

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
DELAYED_DISKS = ["disk1", "disk2", "disk3"]
REMOVE_DESTINATION_DISKS = ["disk1", "disk2"]
REMOVE_SOURCE_DISKS = ["disk3"]


def disk_path(key):
    if key in DELAYED_DISKS:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


def remove_cmd_bg(name):
    # Background the entire `braid remove --yes` invocation so
    # machine.execute returns immediately while the kernel migrates
    # extents in the background. ProgressOutput is "off" by default
    # for non-TTY stderr, so this stays quiet.
    return (
        f"(braid remove {name} --yes) > /tmp/remove.log 2>&1 &"
    )


# --- Phase 1: build a 3-disk RAID1 pool. ---

with subtest("Build 3-disk pool"):
    for name in DELAYED_DISKS:
        dm_delay_create(machine, name)
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write payload before remove"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=100 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: kick off the remove. There is no `btrfs device remove
# status` equivalent to `btrfs replace status -1`, so we drive the LB
# trigger off the kernel exclusive-operation signal. Hard-fail if we
# never see `device remove` so a degraded test does not silently no-op. ---

USED_RE = re.compile(r"\s+Data,RAID1:\s+(\d+)")


def get_disk3_used_bytes():
    raw = machine.execute("btrfs device usage --raw /mnt/storage 2>&1")[1]
    in_disk3 = False
    for line in raw.splitlines():
        if line.startswith("/dev/mapper/braid-disk3"):
            in_disk3 = True
            continue
        if in_disk3:
            if line.strip() == "" or line.startswith("/dev/"):
                # End of disk3 stanza
                break
            m = USED_RE.match(line)
            if m:
                return int(m.group(1))
    return None


def ensure_disk3_relocation_work():
    # btrfs device remove only has data relocation work if the source device
    # owns Data,RAID1 extents. The small payload usually creates that state,
    # but do not rely on allocator placement: allocate bounded filler until
    # btrfs reports disk3 data extents directly.
    for i in range(32):
        used = get_disk3_used_bytes()
        if used is not None and used > 0:
            return used
        machine.succeed(f"fallocate -l 64M /mnt/storage/remove-fill-{i}")
        machine.succeed("sync")

    usage = machine.execute("btrfs device usage --raw /mnt/storage 2>&1")[1]
    raise AssertionError(
        "could not create Data,RAID1 allocation on disk3 before remove:\n"
        f"{usage}"
    )


with subtest("Start remove and wait for in-flight relocation"):
    initial_used = ensure_disk3_relocation_work()
    print(f"disk3 Data,RAID1 used (initial): {initial_used}")
    assert initial_used is not None and initial_used > 0, (
        f"could not read disk3's Data,RAID1 usage before remove\n"
        f"output: {machine.execute('btrfs device usage --raw /mnt/storage 2>&1')[1]}"
    )

    dm_delay_activate(machine, REMOVE_DESTINATION_DISKS, write_delay_ms=500)
    dm_delay_activate(machine, REMOVE_SOURCE_DISKS, read_delay_ms=500)
    machine.execute(remove_cmd_bg("disk3"))

    saw_in_flight = False
    last_exclusive_op = ""
    for _ in range(800):  # 40s budget
        ret = machine.execute(
            "cat /sys/fs/btrfs/*/exclusive_operation 2>&1"
        )
        last_exclusive_op = ret[1]
        if "device remove" in last_exclusive_op.lower():
            saw_in_flight = True
            print(f"exclusive_operation: {last_exclusive_op.strip()}")
            break
        time.sleep(0.05)

    assert saw_in_flight, (
        "Never observed device remove in flight. The remove may have "
        "finished too fast despite dm-delay, or the exclusive_operation "
        "probe fell out of date.\n"
        f"last exclusive_operation:\n{last_exclusive_op}\n"
        f"final usage:\n{machine.execute('btrfs device usage --raw /mnt/storage 2>&1')[1]}"
    )

# --- Phase 3: trigger UPS critical via upsrw. upsmon declares
# critical when ST_ONBATT and ST_LOWBATT are both set
# (reference/nut/clients/upsmon.c:1404). With the fixture's
# FINALDELAY=0 and POLLFREQ=1, SHUTDOWNCMD fires within ~1s. ---

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

# --- Phase 4: reboot. Inspect post-crash state, then run recover. ---

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
    assert "timed out" not in svc_log.lower(), (
        f"braid-online.service was killed by timeout during UPS shutdown.\n"
        f"Journal:\n{svc_log}"
    )

# Whether or not the journal survived depends on a millisecond-level
# race between the kernel's chunk-relocation loop and systemd's
# shutdown sequence. The test design forces the journal to survive in
# the typical case, but the
# matrix's correctness contract is "post-recover state is clean",
# not "the journal definitely existed". Capture which path we took.
journal_existed = (
    machine.execute("test -f /var/lib/braid/pending-op.json")[0] == 0
)
print(f"journal survived the crash: {journal_existed}")

with subtest("Pending-op journal survived the forced shutdown"):
    # We sized the payload generously enough that the remove should be
    # interrupted. If this assertion ever fails consistently, the
    # payload needs to grow -- the matrix loses meaning if the journal
    # path is never exercised.
    assert journal_existed, (
        "remove finished before umount and the journal is gone -- the "
        "matrix test silently degraded to a no-op. Increase the dm-delay "
        "or the relocation-work threshold to widen the in-flight window."
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
    # The remove was interrupted mid-flight. The kernel does NOT
    # persist `device remove` in-flight state; on next mount the
    # device is still a member. Recover writes pre membership and
    # guidance reports "remove did not complete -- re-run".
    assert "remove did not complete" in recover_out, (
        f"recover did not emit 'remove did not complete' guidance.\n"
        f"Output:\n{recover_out}"
    )

# --- Phase 5: assertions about the post-recover state. ---

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

with subtest("Live pool has the pre-remove membership (rollback)"):
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== final btrfs filesystem show ===\n{fi_show}")
    assert "MISSING" not in fi_show, (
        f"phantom MISSING entry after recover:\n{fi_show}"
    )
    assert "Total devices 3" in fi_show, (
        f"post-recover topology should still have 3 devices (the kernel "
        f"does not persist mid-remove state):\n{fi_show}"
    )
    for present in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert present in fi_show, (
            f"{present} missing from live pool after rollback:\n{fi_show}"
        )

with subtest("pool.json reflects the pre-remove membership"):
    final_pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== final pool.json ===\n{final_pool_json}")
    for present in ["disk1", "disk2", "disk3"]:
        assert f'"{present}"' in final_pool_json, (
            f"{present} missing from rolled-back pool.json:\n{final_pool_json}"
        )

with subtest("Journal cleared after recover"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("Payload checksum survived the forced shutdown"):
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_sha}"
    )

with subtest("Operator can re-run braid remove to finish the operation"):
    # Recovery rolled the membership back to pre. The operator's next
    # action is to re-run braid remove. This subtest pins down that
    # the rollback leaves the pool in a state where braid remove can
    # actually start fresh (no leftover paused balance, no leftover
    # mapper wedged in some intermediate state).
    machine.succeed("braid remove disk3 --yes")
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "Total devices 2" in fi_show, (
        f"second remove did not complete:\n{fi_show}"
    )
    assert "braid-disk3" not in fi_show, (
        f"disk3 still in pool after second remove:\n{fi_show}"
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
        f"pool should have 2 devices after the second remove:\n{cycle_fs_show}"
    )

machine.shutdown()
