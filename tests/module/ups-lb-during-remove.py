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
# (plans/wip/forced-shutdown-recovery-proof.md, "Remove" section)
# concluded that the rollback case is well-handled by the existing
# recover code: live device count == pre count, recover writes pre
# membership, recovery_guidance prints "remove did not complete --
# re-run braid remove". This VM test exercises that path end-to-end
# under the actual systemd shutdown sequence the UPS LB trigger
# initiates.
#
# Scenario: Operator started `braid remove disk3` against a 3-disk
# RAID1 pool with several GiB of data. Utility power dropped during
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


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload to make remove measurably slow"):
    # 3000 MiB on tmpfs-backed disks gives the remove ~2s of in-
    # flight relocation work, wider than the ~1s shutdown window. The
    # payload size is bounded above by the ENOSPC preflight (see the
    # disk-sizing comment in the .nix file): too much data and the
    # surviving disks cannot absorb the disk-being-removed's chunks,
    # so braid remove rejects in preflight and the test never
    # exercises the in-flight path.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: kick off the remove. There is no `btrfs device remove
# status` equivalent to `btrfs replace status -1`, so we drive the LB
# trigger off the device-usage signal: poll `btrfs device usage --raw
# /mnt/storage` for disk3 and break once its `Used` count drops below
# the initial (= relocation in flight). Hard-fail if we never see
# in-flight progress so a degraded test does not silently no-op. ---

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


with subtest("Start remove and wait for in-flight relocation"):
    initial_used = get_disk3_used_bytes()
    print(f"disk3 Data,RAID1 used (initial): {initial_used}")
    assert initial_used is not None and initial_used > 0, (
        f"could not read disk3's Data,RAID1 usage before remove\n"
        f"output: {machine.execute('btrfs device usage --raw /mnt/storage 2>&1')[1]}"
    )

    machine.execute(remove_cmd_bg("disk3"))

    saw_in_flight = False
    for _ in range(800):  # 40s budget
        used = get_disk3_used_bytes()
        if used is not None and used < initial_used:
            saw_in_flight = True
            print(
                f"disk3 Data,RAID1 used now: {used} "
                f"(down from {initial_used} -- relocation in flight)"
            )
            break
        if used is None:
            # Disk3 stanza disappeared -- remove finished already.
            break
        time.sleep(0.05)

    assert saw_in_flight, (
        "Never observed disk3 relocation in flight. The remove may have "
        "finished too fast (bump the payload size in the .nix) or the "
        "btrfs device-usage parsing fell out of date.\n"
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
# the typical case (5 GiB payload + 1s shutdown window), but the
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
        "matrix test silently degraded to a no-op. Bump the payload "
        "size in tests/module/ups-lb-during-remove.py to widen the "
        "in-flight window."
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
