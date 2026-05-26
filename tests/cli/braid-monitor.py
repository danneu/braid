# Test: braid monitor + ack lifecycle
#
# Intent: Verify the full alert lifecycle for btrfs-detected issues:
#   detection -> status banner -> ack -> cleared, plus exit 2 for
#   config-load setup errors.
#
# Why it exists: Without this test, we have no integration proof that
#   `braid monitor` exit codes, `braid status` banners, and `braid ack`
#   all agree on the alert state.
#
# Scenario: 3-disk RAID1 pool. Check config-load failures after a healthy
#   monitor proves the pool lock is acquirable. Then close one LUKS mapper
#   to simulate a failed drive. monitor detects the degraded state, status
#   shows the banner, ack clears it, and monitor returns clean.

import json
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
acked_stats_path = "/var/lib/braid/acked-stats.json"


def acked_stats_fingerprint():
    return machine.succeed(
        f"if test -f {acked_stats_path}; then "
        f"stat -c '%i %s %y' {acked_stats_path}; "
        "else printf absent; fi"
    ).strip()


def get_devid(mapper_name):
    """Extract the btrfs devid for a given mapper from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"devid\s+(\d+)", line)
            if m:
                return int(m.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")


# --- Setup: create 3-disk RAID1 pool ---
with subtest("Create 3-disk RAID1 pool"):
    for d in ["disk1", "disk2", "disk3"]:
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/disk/by-id/virtio-{d}"
        )
        machine.succeed(
            f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-{d} braid-{d}"
        )

    machine.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-disk1 /dev/mapper/braid-disk2 /dev/mapper/braid-disk3"
    )
    machine.succeed("mkdir -p /mnt/storage")
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
    machine.succeed("mkdir -p /var/lib/braid")

with subtest("Healthy pool: monitor exits 0"):
    machine.succeed("braid monitor")

with subtest("braid monitor exits 2 on config-load failure (setup error, not lock/alert exit)"):
    # monitor takes the pool lock first (MonitorSilent); lock errors also exit 2.
    # The healthy run above proves the lock is acquirable, so the only changed
    # variable is --config. Assert the error names the config path to confirm
    # exit 2 is the config-load path, not a lock error.
    machine.succeed("echo 'not json {{{' > /tmp/bad.json")
    status, output = machine.execute("braid monitor --config /tmp/bad.json 2>&1")
    assert status == 2, f"unparseable config must exit 2, got {status}: {output}"
    assert "/tmp/bad.json" in output, f"exit 2 must be config-load (not lock), got: {output}"
    status, output = machine.execute("braid monitor --config /tmp/nonexistent.json 2>&1")
    assert status == 2, f"missing config must exit 2, got {status}: {output}"
    assert "/tmp/nonexistent.json" in output, f"exit 2 must be config-load (not lock), got: {output}"

with subtest("Healthy pool: status has no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT in healthy status, got: {output}"

# /*
#  * Intent: A mounted healthy-pool `braid ack` with no active alert source
#  *   exits 0 without mutating acked-stats.json.
#  * Why it exists: A proactive no-op ack used to snapshot current btrfs
#  *   counters as the new baseline, which could bury errors that occurred
#  *   after the last monitor cycle but before ack.
#  * Scenario: the pool is mounted and healthy; the user runs `braid ack`
#  *   out of caution before any monitor cycle has latched an alert.
#  */
with subtest("Healthy mounted pool: ack is a durable no-op"):
    before = acked_stats_fingerprint()
    output = machine.succeed("braid ack")
    assert "no active alerts" in output, (
        f"Expected no active alerts from healthy ack, got: {output}"
    )
    after = acked_stats_fingerprint()
    assert after == before, (
        f"Expected acked-stats fingerprint unchanged, before={before!r} after={after!r}"
    )

with subtest("Seed pool.json membership before failure"):
    members = {}
    devids_by_name = {}
    for name in ["disk1", "disk2", "disk3"]:
        luks_uuid = machine.succeed(
            f"cryptsetup luksUUID /dev/disk/by-id/virtio-{name}"
        ).strip()
        devid = get_devid(f"braid-{name}")
        members[luks_uuid] = {
            "name": name,
            "by_id": f"/dev/disk/by-id/virtio-{name}",
            "devid": devid,
            "added_at": "2024-01-01T00:00:00Z",
        }
        devids_by_name[name] = devid
    pool_json = json.dumps(
        {"disks": members}, sort_keys=True, separators=(",", ":")
    )
    machine.succeed(
        f"printf '%s' {shlex.quote(pool_json)} > /var/lib/braid/pool.json"
    )
    disk2_devid = devids_by_name["disk2"]

# --- Simulate disk failure: close one LUKS mapper ---
with subtest("Simulate disk failure"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    # Remount degraded (only 2 of 3 devices)
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

with subtest("Degraded pool: monitor exit code is exactly 1"):
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"

with subtest("Degraded pool: latch file created"):
    machine.succeed("test -f /var/lib/braid/alert-latch.json")

with subtest("Degraded pool: status shows ALERT banner"):
    output = machine.succeed("braid status")
    assert "ALERT -- disk health issue detected." in output, (
        f"Expected ALERT in degraded status, got: {output}"
    )
    assert "braid ack" in output, f"Expected 'braid ack' hint in status, got: {output}"
    assert f"missing device: disk2 (devid {disk2_devid})" in output, (
        f"Expected 'missing device: disk2 (devid {disk2_devid})' cause in status, got: {output}"
    )

with subtest("Degraded pool: status --json shows alert"):
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "missing_device" in cause_types, f"Expected missing_device cause, got: {cause_types}"
    missing_causes = [
        c for c in report["alert_causes"]
        if c["type"] == "missing_device" and c.get("devid") == disk2_devid
    ]
    assert missing_causes, (
        f"Expected missing_device cause with devid={disk2_devid}, got: {report['alert_causes']}"
    )

with subtest("Ack clears alert"):
    machine.succeed("braid ack")
    # Verify acked state file was written
    machine.succeed("test -f /var/lib/braid/acked-stats.json")
    acked = json.loads(machine.succeed("cat /var/lib/braid/acked-stats.json"))
    # Find the entry with missing_acked = true
    has_missing_acked = any(
        v.get("missing_acked", False) for v in acked.values()
    )
    assert has_missing_acked, f"Expected missing_acked=true in acked stats, got: {acked}"

with subtest("Ack removes latch file"):
    machine.fail("test -f /var/lib/braid/alert-latch.json")

with subtest("After ack: status has no ALERT"):
    output = machine.succeed("braid status")
    assert "ALERT" not in output, f"Expected no ALERT after ack, got: {output}"

with subtest("After ack: monitor exits 0"):
    machine.succeed("braid monitor")

# Intent: A corrupt /var/lib/braid/alert-latch.json must surface as a loud
#   alert rather than silently rebuilding into an empty latch.
# Why it exists: The prior load_alert_latch returned None on parse failure,
#   conflating "absent" with "corrupt". cmd_monitor would then merge live
#   causes onto an empty slate and overwrite the corrupt file -- silently
#   dropping previously-latched-but-now-cleared causes and violating the
#   "latched until ack" invariant. status would also report "no alert" so
#   the operator never noticed.
# Scenario: external tampering or filesystem damage corrupts the latch
#   while the pool is mounted and healthy. monitor must exit 1 (not 0,
#   not 2), the corrupt bytes must be preserved in the .corrupt sidecar,
#   status must surface the corruption, and ack must clear both files.
with subtest("Corrupt latch (mounted): monitor surfaces and quarantines"):
    # Pool is currently mounted and healthy; no real alert exists.
    machine.succeed("printf 'not json' > /var/lib/braid/alert-latch.json")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected monitor exit 1 on corrupt latch, got {rc}"
    # Corrupt bytes preserved in sidecar
    machine.succeed("test -f /var/lib/braid/alert-latch.json.corrupt")
    sidecar = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
    assert sidecar == "not json", f"Expected sidecar to hold original bytes, got: {sidecar!r}"
    # status exits 0 (status never returns non-zero on alerts) but surfaces it
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "computation_error" in cause_types, (
        f"Expected computation_error cause, got: {cause_types}"
    )
    ce_details = [c.get("detail", "") for c in report["alert_causes"] if c["type"] == "computation_error"]
    assert any("alert latch" in d for d in ce_details), (
        f"Expected ComputationError detail to mention 'alert latch', got: {ce_details}"
    )
    # ack clears both the live latch and the .corrupt sidecar
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")

# Intent: When the alert latch becomes corrupt a second time before ack, the
#   first .corrupt sidecar must be preserved -- ADR 014 guarantees the bad
#   bytes survive for forensics until ack, and the first corruption event is
#   the most valuable snapshot.
# Why it exists: Pre-fix, std::fs::rename atomically replaced the .corrupt
#   sidecar on every quarantine, silently destroying the original failure
#   event's bytes whenever a second corruption occurred before braid ack.
# Scenario: Operator misses the first ALERT; meanwhile the latch corrupts
#   again (FS damage, manual edit, slow tampering). The second monitor cycle
#   must keep the first sidecar and surface the lost-evidence condition in
#   braid status's JSON output.
with subtest("Repeated corrupt latch preserves first sidecar"):
    machine.succeed("printf 'first corruption' > /var/lib/braid/alert-latch.json")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected monitor exit 1 on first corrupt latch, got {rc}"
    first_sidecar = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
    assert first_sidecar == "first corruption"

    # Second corruption: overwrite the freshly written valid latch.
    machine.succeed("printf 'second corruption' > /var/lib/braid/alert-latch.json")
    rc2 = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc2 == "1", f"Expected monitor exit 1 on second corrupt latch, got {rc2}"

    # Sidecar still holds the FIRST event's bytes.
    preserved = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
    assert preserved == "first corruption", (
        f"first sidecar must survive second quarantine, got {preserved!r}"
    )

    # braid status surfaces the lost-evidence condition.
    status_json = machine.succeed("braid status --json")
    assert "prior alert-latch.json.corrupt sidecar exists" in status_json

    # ack clears both files as before.
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")

# Intent: An offline `braid ack` of a latched MissingDevice cause persists
#   missing_acked=true into acked-stats.json so the alert does not re-fire
#   on the next monitor cycle after the pool is remounted.
# Why it exists: Without this, ack_offline only deleted the latch and
#   smartd flag, never updating acked-stats. The next monitor cycle saw the
#   device still missing, no missing_acked entry, and re-latched the same
#   MissingDevice cause -- making the operator hear the beeper again right
#   after they thought they had silenced it. The cycle "lock -> ack ->
#   unlock -> mount -> monitor exit 0" is the regression gate; checking
#   only "ack succeeds and latch is gone" is not enough because the next
#   monitor invocation against an unchanged baseline would silently re-fire.
# Scenario: pool is degraded with devid 2 missing; user locks the pool,
#   acks offline, then re-opens the surviving disks and remounts.
with subtest("MissingDevice alert acked offline does not re-fire on remount"):
    # Pool is still degraded. Remove acked state to re-trigger alert.
    machine.succeed("rm -f /var/lib/braid/acked-stats.json")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"
    machine.succeed("test -f /var/lib/braid/alert-latch.json")
    # Now lock the pool
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk3")
    # Status should still show the latched alert
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in offline status, got: {output}"
    # Offline ack should succeed and persist missing_acked into acked-stats
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.succeed("test -f /var/lib/braid/acked-stats.json")
    acked = json.loads(machine.succeed("cat /var/lib/braid/acked-stats.json"))
    has_missing_acked = any(v.get("missing_acked", False) for v in acked.values())
    assert has_missing_acked, (
        f"Expected missing_acked=true after offline ack, got: {acked}"
    )
    # Re-unlock surviving disks and remount degraded; monitor must NOT re-fire.
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-disk1 braid-disk1"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup open --type luks --key-file=- /dev/disk/by-id/virtio-disk3 braid-disk3"
    )
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
    assert rc == "0", (
        f"Expected monitor exit 0 after offline ack + remount (no re-fire), got {rc}"
    )
    machine.fail("test -f /var/lib/braid/alert-latch.json")

# Intent: Offline `braid ack` refuses with a non-zero exit when the latch
#   contains any BtrfsDeviceErrors cause -- even when mixed with a
#   MissingDevice cause that would otherwise be acceptable. The latch and
#   acked-stats.json must both be untouched (all-or-nothing atomicity).
# Why it exists: BtrfsDeviceErrors silencing requires a counter baseline
#   captured from live `btrfs device stats` output, which is impossible
#   with the pool locked. A buggy partial-apply that marked
#   missing_acked=true before checking for BtrfsDeviceErrors would leave
#   the user in an inconsistent state ("I acked but it still says ALERT").
#   Starting from acked-stats.json absent makes a partial-apply visible:
#   the file would appear.
# Scenario: a hand-crafted latch fixture simulates a pool that had btrfs
#   device errors plus a missing device. The pool is locked. `braid ack`
#   must refuse and tell the operator to unlock first.
with subtest("Offline ack refused on mixed BtrfsDeviceErrors + MissingDevice latch"):
    # Lock the pool again (extended subtest above left it mounted).
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk3")
    # Reset acked-stats so partial-apply would be visible.
    machine.succeed("rm -f /var/lib/braid/acked-stats.json")
    # Hand-write a full AlertState fixture (load_alert_latch deserializes
    # AlertState, not a bare AlertCause -- a bare cause object would
    # exercise the corrupt-latch path instead of the refusal path).
    latch_fixture = json.dumps(
        {
            "causes": [
                {"type": "btrfs_device_errors", "devid": 1},
                {"type": "missing_device", "devid": 2},
            ],
        }
    )
    machine.succeed(
        f"printf '%s' '{latch_fixture}' > /var/lib/braid/alert-latch.json"
    )
    # Run ack; expect failure with the actionable error message.
    ack_result = machine.execute("braid ack 2>&1")
    ack_exit = ack_result[0]
    ack_stderr = ack_result[1]
    assert ack_exit != 0, (
        f"Expected non-zero exit on mixed-cause refusal, got {ack_exit}"
    )
    assert "unlock the pool first" in ack_stderr, (
        f"Expected 'unlock the pool first' in stderr, got: {ack_stderr}"
    )
    # Latch byte-identical to the fixture (refusal must not delete or rewrite).
    on_disk = machine.succeed("cat /var/lib/braid/alert-latch.json").rstrip("\n")
    assert on_disk == latch_fixture, (
        f"Expected latch bytes preserved, got: {on_disk!r}"
    )
    # acked-stats still absent (no partial application).
    machine.fail("test -f /var/lib/braid/acked-stats.json")
    # Clean up the fixture so the next subtest starts from a known state.
    machine.succeed("rm -f /var/lib/braid/alert-latch.json")

# Intent: A corrupt latch must be ack-able even with the pool offline.
# Why it exists: If `latch_count = 0` is set on parse failure (the naive
#   fix) and smartd is inactive, ack_offline gates on
#   `has_alert = latch_count > 0 || smartd_active` and returns
#   PoolNotMounted, leaving the corrupt file on disk forever. The user
#   would have no way to clear it without remounting.
# Scenario: pool is offline (already the case at this point in the
#   script), and the latch on disk is unparseable. `braid ack` must
#   succeed and remove both the live latch and the .corrupt sidecar.
with subtest("Corrupt latch (offline): ack clears it without PoolNotMounted"):
    # Pool is currently offline (cryptsetup close was called above).
    # Both alert-latch.json and alert-latch.json.corrupt should be absent
    # at this point; create just the live (corrupt) file.
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.succeed("printf 'not json' > /var/lib/braid/alert-latch.json")
    rc = machine.succeed("set +e; braid ack; echo $?").strip().splitlines()[-1]
    assert rc == "0", f"Expected ack exit 0 on offline corrupt latch, got {rc}"
    machine.fail("test -f /var/lib/braid/alert-latch.json")
    machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")

machine.shutdown()
