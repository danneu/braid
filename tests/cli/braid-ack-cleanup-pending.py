# Test: ack cleanup-pending cross-command contract
#
# Intent: Drive the full produce -> surface -> consume cycle for the
#   alert-cleanup-pending sentinel over real files and the real `braid
#   ack` / `braid status` binaries: a mid-cleanup ack failure leaves the
#   sentinel, `braid status` surfaces it as an alert cause, and a
#   sentinel-only `braid ack` re-enters cleanup and clears it.
#
# Why it exists: both halves of this contract (status surfacing in
#   status.rs#resolve_alert_state and the sentinel-only retry branch in
#   ack.rs#cmd_ack_impl) are heavily unit-tested, but every unit test runs
#   against an isolated temp dir with an injected runner. None drives the
#   production /var/lib/braid path through the real binaries. A wiring
#   regression -- a path mismatch, a renderer that drops the cause, a
#   producer that marks the wrong file -- would pass the entire unit suite.
#   The contract shipped in c0360184 with unit tests only; no VM test
#   followed.
#
# Scenario: Healthy 2-disk RAID1 pool. A smartd flag makes a mounted ack
#   reach cleanup; alert-latch.json.corrupt is poisoned as a directory so
#   remove_alert_latch_corrupt fails with EISDIR *after* the sentinel was
#   marked and the smartd flag removed. ack exits 1 with the sentinel and
#   the persisted baseline surviving. `braid status` reports the
#   cleanup-pending cause. The operator removes the poison directory and
#   re-runs `braid ack`, which re-enters cleanup directly and clears the
#   sentinel.


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

# --- Setup: create 2-disk RAID1 pool ---
with subtest("Create 2-disk RAID1 pool"):
    for d in ["disk1", "disk2"]:
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-{d}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-{d} braid-{d}"
        )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

with subtest("Healthy pool: no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT, got: {output}"

# Produce: force a cleanup failure after the sentinel is marked. The smartd
# flag drives a mounted ack past the `no active alerts` no-op into the
# save-baseline + cleanup path; the poison directory at
# alert-latch.json.corrupt makes remove_alert_latch_corrupt fail with EISDIR.
# This is the persist-before-maintenance invariant: save_acked_stats and the
# smartd-flag removal both run before the forced failure, and the sentinel
# survives.
with subtest("ack fails mid-cleanup, leaving the sentinel"):
    machine.succeed("touch /var/lib/braid/smartd-alert")
    machine.succeed("mkdir /var/lib/braid/alert-latch.json.corrupt")

    # Assert exit code 1 exactly (not just nonzero): the Commands::Ack arm in
    # main.rs maps every ack error to exit 1, so a dispatcher regression to
    # exit 2 (or any other code) would be a real bug. Deterministic here --
    # the config exists and the pool lock is free, so the forced CleanupFailed
    # is the only error path. Capture streams separately because the failing
    # ack emits a `systemctl stop braid-alert.service` warning on stderr
    # (braid-alert.service is not installed in this VM).
    rc, _ = machine.execute(
        "braid ack >/tmp/ack-fail.out 2>/tmp/ack-fail.err"
    )
    assert rc == 1, f"expected braid ack to exit 1, got {rc}"

    # Sentinel produced before the failed maintenance step.
    machine.succeed("test -f /var/lib/braid/alert-cleanup-pending")
    # Baseline persisted before cleanup ran (save_acked_stats precedes it).
    machine.succeed("test -f /var/lib/braid/acked-stats.json")
    # Cleanup got past the smartd-flag removal, proving the failure is at the
    # later .corrupt step, not earlier.
    machine.fail("test -f /var/lib/braid/smartd-alert")

# Surface: status reads the sentinel as a cleanup-pending alert cause. The
# exact string pins the docs/commands/ack.md messaging invariant. No SMART alert
# cause must appear -- the flag was already removed, so the sentinel is the only
# surfaced cause. status exits 0 even with an active alert, so succeed() is
# correct here.
with subtest("status reports the cleanup-pending cause"):
    out = machine.succeed("braid status")
    assert "ALERT" in out, f"expected ALERT, got: {out}"
    assert "ack cleanup pending -- re-run `braid ack` to resume" in out, (
        f"expected the cleanup-pending cause string, got: {out}"
    )
    assert "SMART health warning" not in out, (
        f"smartd flag was removed; no SMART alert cause should surface, got: {out}"
    )

# Consume: the operator removes the poison and re-runs ack. The sentinel-only
# branch in cmd_ack_impl is hoisted above probe_pool_alerts, so it re-enters
# cleanup directly and prints the documented confirmation. Capture stdout
# separately so the systemctl-stop warning on stderr does not pollute it.
with subtest("retry clears the sentinel"):
    machine.succeed("rmdir /var/lib/braid/alert-latch.json.corrupt")
    rc, _ = machine.execute(
        "braid ack >/tmp/ack-retry.out 2>/tmp/ack-retry.err"
    )
    assert rc == 0, f"expected braid ack retry to exit 0, got {rc}"
    stdout = machine.succeed("cat /tmp/ack-retry.out")
    assert stdout == "acknowledged current alerts\n", (
        f"expected documented sentinel-only retry output, got: {stdout!r}"
    )
    machine.fail("test -f /var/lib/braid/alert-cleanup-pending")

with subtest("After retry: no ALERT"):
    out = machine.succeed("braid status")
    assert "ALERT" not in out, f"expected no ALERT after retry, got: {out}"
    assert "ack cleanup pending" not in out, (
        f"expected no cleanup-pending cause after retry, got: {out}"
    )

machine.shutdown()
