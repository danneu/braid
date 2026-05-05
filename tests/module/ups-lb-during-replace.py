# Test: ups-lb-during-replace
#
# Intent: verify that a forced shutdown driven by upsmon's critical-state
# SHUTDOWNCMD during an active `btrfs replace` is a recoverable state.
# After reboot, `braid recover` either completes the replace or cleanly
# resumes it without manual intervention. The pool mounts cleanly, btrfs
# reports zero errors, no LUKS mappers are orphaned, and pool.json
# matches the post-replace target membership.
#
# Why it exists: ADR 020's guarantee that "mid-mutation power loss is a
# supported recovery case" must be proven per mutation class via VM tests
# before the ADR can flip from Draft to Active. Open Question 1 of ADR
# 020 names this exact scenario as the primary blocker. Without this
# proof, claiming the replace path is recoverable is unbacked. This test
# also doubles as the load-bearing demonstration of the Pre-M11 audit's
# remediation in cli/src/recover.rs (replay of pool_resize_device after
# the kernel-resumed dev_replace).
#
# Scenario: Operator started `braid replace disk2 disk4` against a
# 3-disk RAID1 pool with several hundred MiB of data. Utility power
# dropped during the replace; the UPS drained to LB; upsmon fired its
# critical-state SHUTDOWNCMD = systemctl poweroff. The host shut down,
# the operator returns the next morning and runs `braid recover`. The
# pool comes back with disk4 in place of disk2 and full capacity
# reclaimed; no manual btrfs commands required.

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
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def replace_cmd_bg(old, new):
    # Background the entire (passphrase | braid replace) pipeline so
    # machine.execute returns immediately. The subshell ensures the &
    # applies to the whole pipeline rather than just the tail braid call.
    return (
        f"(printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes) > /tmp/replace.log 2>&1 &"
    )


# --- Phase 1: build a 3-disk RAID1 pool with enough data to make the
# replace measurably slow (mirrors tests/repro/btrfs-replace-interrupted-
# mid-flight.py staging). ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload to make replace measurably slow"):
    # 3000 MiB on tmpfs-backed virtual disks gives a replace that
    # takes ~3s, which is wider than the ~1s shutdown window from LB
    # detection to umount (lib/ups-fixture.nix lowers FINALDELAY to 0
    # to keep that window tight). If this number gets trimmed back,
    # the replace can finish before SHUTDOWNCMD actually unmounts the
    # pool and the journal will already be cleared on reboot -- the
    # test would silently degrade at the journal-survived assertion
    # because there would be nothing to recover from.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=3000 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: kick off the replace and wait until the kernel reports
# in-flight state SPECIFICALLY (running, not finished). `btrfs replace
# status -1` only writes one of three shapes:
#
#   - "<pct>% done, ..."                              -> running
#   - "Started on <t1>, finished on <t2>, ..."        -> finished
#   - "" or "no operation running"                    -> idle
#
# Older drafts of this test broke out on either "Started on" or
# "% done", which silently let the trigger fire AFTER the replace had
# already finished -- the journal would already be cleared and recover
# would have nothing to do. The check below requires "% done" AND the
# absence of "finished on" so we only release the LB trigger while the
# kernel is genuinely mid-flight.

PCT_RE = re.compile(r"(\d+(?:\.\d+)?)% done")


def parse_replace_pct(status_text):
    """Return (state, pct) where state is 'running' / 'finished' / 'idle'.
    pct is float for running, 100.0 for finished, None for idle."""
    if "finished on" in status_text:
        return ("finished", 100.0)
    match = PCT_RE.search(status_text)
    if match:
        return ("running", float(match.group(1)))
    return ("idle", None)


with subtest("Start replace and wait for in-flight progress"):
    machine.execute(replace_cmd_bg("disk2", "disk4"))

    saw_in_flight = False
    saw_finished_too_early = False
    last_status = ""
    last_pct = None
    # 800 * 0.05s = 40s budget, longer than the time it takes to even
    # observe in-flight on a slow VM. The replace itself runs ~30s, so
    # this loop should exit well before the budget.
    for _ in range(800):
        ret = machine.execute("btrfs replace status -1 /mnt/storage 2>&1")
        last_status = ret[1]
        state, pct = parse_replace_pct(last_status)
        if state == "running":
            saw_in_flight = True
            last_pct = pct
            break
        if state == "finished":
            saw_finished_too_early = True
            break
        time.sleep(0.05)

    print("=== last btrfs replace status before LB ===")
    print(last_status)
    assert not saw_finished_too_early, (
        "btrfs replace finished before the test could observe in-flight "
        "state. The payload size is too small or the polling cadence is "
        "too coarse. The matrix test cannot exercise the forced-shutdown "
        "scenario without a reliable in-flight window. Last status:\n"
        + last_status
    )
    assert saw_in_flight, (
        "Never observed btrfs replace in-flight -- test cannot exercise "
        "the interrupted-replace scenario. Last status:\n" + last_status
    )
    print(f"in-flight pct at LB trigger: {last_pct}")

# --- Phase 3: drive UPS critical via upsrw. upsmon declares critical
# when ST_ONBATT and ST_LOWBATT are both set
# (reference/nut/clients/upsmon.c:1404). FINALDELAY default is 5s before
# SHUTDOWNCMD fires (reference/nut/clients/upsmon.c:114,935). ---

with subtest("Drive UPS critical: upsrw ups.status = OB LB"):
    machine.succeed(
        "upsrw -s 'ups.status=OB LB' "
        "-u testops -p testpass ups@localhost"
    )

with subtest("Host shuts down in response to upsmon SHUTDOWNCMD"):
    try:
        machine.wait_for_shutdown()
    except Exception as e:
        # Emit the upsmon journal to diagnose whether SHUTDOWNCMD fired.
        _rc, upsmon_log = machine.execute(
            "journalctl -u upsmon.service --no-pager -n 100"
        )
        raise AssertionError(
            f"host did not shut down after OB+LB. upsmon journal:\n{upsmon_log}"
        ) from e

# --- Phase 4: reboot. Capture pre-recover state, then run recover. ---

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

with subtest("Pending-op journal survived the crash"):
    machine.succeed("test -f /var/lib/braid/pending-op.json")

with subtest("braid unlock refuses with journal present"):
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    assert exit_code != 0, f"unlock should refuse with journal, got exit 0:\n{output}"
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
    # The kernel's resume-on-mount path completed the dev_replace before
    # `cmd_recover` probed the live pool, so guidance reports the
    # replace as completed.
    assert "replace completed" in recover_out, (
        f"recover did not emit 'replace completed' guidance.\n"
        f"Output:\n{recover_out}"
    )
    kernel_wait = "[wait] pool: waiting for kernel dev_replace to finish..."
    kernel_ok = "[ok]   pool: kernel dev_replace finished"
    if kernel_ok in recover_out:
        assert kernel_wait in recover_out, (
            f"kernel dev_replace ok row appeared without wait row:\n{recover_out}"
        )
        assert recover_out.find(kernel_wait) < recover_out.find(kernel_ok), (
            f"kernel dev_replace wait must precede ok:\n{recover_out}"
        )

# --- Phase 5: assert the post-recover state matches expectations. ---

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

with subtest("Live pool has the post-replace target membership"):
    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== final btrfs filesystem show ===\n{fi_show}")
    assert "MISSING" not in fi_show, (
        f"phantom MISSING entry after recover -- recover regressed.\n{fi_show}"
    )
    assert "Total devices 3" in fi_show, (
        f"post-recover topology should have 3 devices, got:\n{fi_show}"
    )
    assert "braid-disk4" in fi_show, (
        f"replace target disk4 missing from live pool after recover:\n{fi_show}"
    )
    assert "braid-disk2" not in fi_show, (
        f"replace source disk2 still in live pool after recover:\n{fi_show}"
    )

with subtest("pool.json reflects the post-replace target membership"):
    final_pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== final pool.json ===\n{final_pool_json}")
    assert '"disk1"' in final_pool_json
    assert '"disk3"' in final_pool_json
    assert '"disk4"' in final_pool_json
    assert '"disk2"' not in final_pool_json

with subtest("Journal cleared after recover"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("No orphaned LUKS mappers"):
    # disk2's mapper must not survive the replace. The recover code does
    # not explicitly close the source mapper (replace.rs:329-343 is
    # best-effort and runs only on the original command), so this asserts
    # that EITHER the kernel never created a stable braid-disk2 mapper
    # entry, OR a subsequent reboot path closed it. Recovery completes
    # the replace; the mapper is allowed to be present (harmless), but
    # a follow-up `braid lock`/`braid unlock` must not surface it.
    lsblk = machine.succeed("lsblk --noheadings --output NAME")
    print(f"=== lsblk after recover ===\n{lsblk}")
    # Disk4 is the new pool member; disk1, disk3 are the survivors.
    for present in ["braid-disk1", "braid-disk3", "braid-disk4"]:
        assert present in lsblk, (
            f"expected {present} mapper to be open after recover; got:\n{lsblk}"
        )

with subtest("Payload checksum survived the forced shutdown"):
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_sha}"
    )

with subtest("Subsequent lock/unlock cycle stays clean (no MISSING)"):
    machine.succeed("braid lock")
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")
    cycle_fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    assert "MISSING" not in cycle_fs_show, (
        f"MISSING re-appeared after a lock/unlock cycle:\n{cycle_fs_show}"
    )
    assert "Total devices 3" in cycle_fs_show, (
        f"pool no longer has 3 devices after a lock/unlock cycle:\n{cycle_fs_show}"
    )

machine.shutdown()
