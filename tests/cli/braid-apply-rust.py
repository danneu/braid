import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def init_disk(by_id, extra="", confirm=""):
    passphrase_q = shlex.quote(passphrase)
    env = f"BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"printf '%s\\n' {passphrase_q} | {env} braid init-disk --config /tmp/braid-config.json --passphrase-stdin {by_id} {extra}"


def apply(extra="", confirm=""):
    passphrase_q = shlex.quote(passphrase)
    env = ""
    if confirm:
        env = f"BRAID_CONFIRM='{confirm}' "
    return f"printf '%s\\n' {passphrase_q} | {env}braid apply --config /tmp/braid-config.json --passphrase-stdin {extra}"


# --- Subtest 0: Setup — init 2 disks, build pool ---

with subtest("Setup: init 2 disks"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))

# --- Subtest 1: Fresh apply builds 2-disk RAID1 ---

with subtest("Fresh apply builds 2-disk RAID1"):
    output = machine.succeed(apply())
    print(f"Apply output:\n{output}")
    assert "Applied" in output, f"Expected Applied in output:\n{output}"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

    machine.succeed("echo 'important data' > /mnt/storage/precious.txt && sync")

# --- Subtest 2: No-op apply ---

with subtest("No-op apply"):
    output = machine.succeed(apply())
    assert "nothing to do" in output.lower() or "no actions" in output.lower(), (
        f"Expected no-op message:\n{output}"
    )

# --- Subtest 3: Add disk3 ---

with subtest("Add disk3 to pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    output = machine.succeed(apply())
    assert "Applied" in output and "skipped" in output, (
        f"Expected footer with Applied/skipped:\n{output}"
    )
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not in pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

# --- Subtest 4: Data intact after add ---

with subtest("Data intact after add"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

# --- Subtest 5: Checkpoint removed after success ---

with subtest("Checkpoint removed after success"):
    machine.fail("test -f /var/lib/braid/apply-state.json")

# --- Subtest 6: History file written ---

with subtest("History file written"):
    machine.succeed("ls /var/lib/braid/history/ | head -1")

# --- Subtest 7: Remove disk3 ---

with subtest("Remove disk3 from pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(apply())

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    machine.fail("test -e /dev/mapper/virtio-disk3")

# --- Subtest 8: Redundancy refusal without confirmation ---

with subtest("Redundancy refusal without confirmation"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(apply())

# --- Subtest 9: Redundancy acceptance with phrase ---

with subtest("Redundancy acceptance with correct phrase"):
    machine.succeed(apply(confirm="remove this disk without redundancy"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device:\n{fi_show}"

# --- Subtest 10: Absent disk warns but continues ---

with subtest("Absent disk warns but continues"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    # disk3 still has LUKS from earlier init
    output = machine.succeed(apply())
    # Should have warning about absent disk99
    assert "warning" in output.lower() or "skip" in output.lower() or "absent" in output.lower(), (
        f"Expected warning about absent disk:\n{output}"
    )
    assert "Applied" in output and "skipped" in output, (
        f"Expected footer with Applied/skipped:\n{output}"
    )
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not added despite absent disk99:\n{fi_show}"

# --- Subtest 11: Replace dead disk ---

with subtest("Setup: simulate disk failure for replace"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk3")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

with subtest("Replace dead disk with --allow-remove-missing"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    # disk2 still has LUKS from earlier init
    machine.succeed(apply(
        extra="--allow-remove-missing",
        confirm="remove missing device from pool",
    ))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk2" in fi_show, f"disk2 not in pool:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"Still has missing:\n{fi_show}"

with subtest("Data intact after replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

# --- Subtest 12: Blocked plan exits 1 ---

with subtest("Setup: build 3-disk pool for ambiguity test"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk4"))
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 not in pool:\n{fi_show}"

with subtest("Blocked plan exits 1"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    machine.fail(apply())

# --- Subtest 13: --allow-remove-ambiguous with confirmation ---

with subtest("--allow-remove-ambiguous with confirmation"):
    output = machine.succeed(apply(
        extra="--allow-remove-ambiguous",
        confirm="remove despite ambiguous identity",
    ))
    assert "Applied" in output, f"Expected successful apply:\n{output}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" not in fi_show, f"disk4 still in pool:\n{fi_show}"

# --- Subtest 14: Semicolon multi-confirmation ---

with subtest("Semicolon multi-confirmation"):
    # Pool: disk1 + disk2. Config: [disk1, disk99_absent]
    # Needs both ambiguity + redundancy phrases
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    output = machine.succeed(apply(
        extra="--allow-remove-ambiguous",
        confirm="remove despite ambiguous identity;remove this disk without redundancy",
    ))
    assert "Applied" in output, f"Expected successful apply:\n{output}"
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk2" not in fi_show, f"disk2 still in pool:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device:\n{fi_show}"

# --- Rebuild for checkpoint tests ---

with subtest("Setup: rebuild 2-disk pool for checkpoint tests"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2", extra="--force", confirm="reformat this disk"))
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk2" in fi_show, f"disk2 not in pool:\n{fi_show}"
    assert "RAID1" in machine.succeed("btrfs fi df /mnt/storage")

# --- Subtest 15: Interrupted apply leaves checkpoint ---

with subtest("Interrupted apply leaves checkpoint"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3", extra="--force", confirm="reformat this disk"))
    cmd = (
        f"printf '%s\\n' {shlex.quote(passphrase)} | "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json --passphrase-stdin"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    checkpoint = json.loads(machine.succeed("cat /var/lib/braid/apply-state.json"))
    completed = [a for a in checkpoint["actions"] if a["status"] == "completed"]
    assert len(completed) >= 1, f"Expected at least 1 completed action:\n{checkpoint}"

# --- Subtest 16: Resume continues from checkpoint ---

with subtest("Resume continues from checkpoint"):
    machine.succeed(apply("--resume"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not in pool after resume:\n{fi_show}"
    machine.fail("test -f /var/lib/braid/apply-state.json")

# --- Subtest 17: Stale checkpoint refuses resume ---

with subtest("Stale checkpoint refuses resume"):
    fake_checkpoint = json.dumps({
        "plan_id": "fake",
        "mount_point": "/mnt/storage",
        "status": "applicable",
        "config_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "last_completed_action_id": "",
        "is_bootstrap": False,
        "actions": [],
        "warnings": [],
        "confirmations": [],
    })
    machine.succeed("mkdir -p /var/lib/braid")
    escaped = fake_checkpoint.replace("'", "'\\''")
    machine.succeed(f"echo '{escaped}' > /var/lib/braid/apply-state.json")
    machine.fail(apply("--resume"))
    machine.succeed("rm /var/lib/braid/apply-state.json")

# --- Subtest 18: Resume target absent → exit 1 ---

with subtest("Resume target absent exits 1"):
    # Current pool: disk1 + disk2 + disk3. Add disk4 then interrupt.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk4", extra="--force", confirm="reformat this disk"))
    cmd = (
        f"printf '%s\\n' {shlex.quote(passphrase)} | "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json --passphrase-stdin"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Hide disk4 to simulate absence
    machine.succeed("cryptsetup close virtio-disk4 || true")
    machine.succeed("mv /dev/disk/by-id/virtio-disk4 /dev/disk/by-id/virtio-disk4.hidden")

    # Resume should fail
    machine.fail(apply("--resume"))
    # Checkpoint should be preserved
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Cleanup
    machine.succeed("mv /dev/disk/by-id/virtio-disk4.hidden /dev/disk/by-id/virtio-disk4")
    machine.succeed("rm -f /var/lib/braid/apply-state.json")

# --- Subtest 19: Apply never calls luksFormat ---

with subtest("Apply never calls luksFormat"):
    # The ActionType enum has no format variant — verified at compile time in plan.rs
    # (plan_no_format_action_exists test). Here we verify via plan JSON that no
    # action of type containing "FORMAT" appears.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    raw = machine.succeed("braid plan --config /tmp/braid-config.json --json")
    p = json.loads(raw)
    for a in p["actions"]:
        assert "FORMAT" not in a["type"], (
            f"ActionType contains FORMAT — safety invariant violated: {a}"
        )

# --- Subtest 20: Existing-but-unmounted pool must NOT trigger mkfs ---
# Bug: probe_pool returns total_devices=0 when unmounted, so
# is_bootstrap = !pool.mounted && pool.total_devices == 0 is true even
# for an existing pool. This causes mkfs.btrfs to wipe the filesystem.

with subtest("Setup: ensure clean 2-disk RAID1 for bootstrap-safety test"):
    # Clean up any stale state from previous subtests
    machine.succeed("rm -f /var/lib/braid/apply-state.json")
    # After earlier subtests the pool has disk1+disk2+disk3. Shrink to 2-disk.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk1" in fi_show, f"disk1 not in pool:\n{fi_show}"
    assert "virtio-disk2" in fi_show, f"disk2 not in pool:\n{fi_show}"
    assert "RAID1" in machine.succeed("btrfs fi df /mnt/storage")
    # Write sentinel data
    machine.succeed("echo 'do not destroy me' > /mnt/storage/sentinel.txt && sync")

with subtest("Existing-but-unmounted pool must not be treated as bootstrap"):
    # Capture the filesystem UUID before unmounting
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    import re
    uuid_match = re.search(r'uuid:\s+(\S+)', fi_show)
    assert uuid_match, f"Could not find UUID in btrfs fi show output:\n{fi_show}"
    original_uuid = uuid_match.group(1)
    print(f"Original fs UUID: {original_uuid}")

    # Unmount and close all mappers — pool exists on disk but is fully offline
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk1")
    machine.succeed("cryptsetup close virtio-disk2")

    # Now add disk3 to force a mutation so apply actually runs actions.
    # disk3 still has its LUKS header from earlier subtests.
    machine.succeed("cryptsetup close virtio-disk3 || true")
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))

    # Run apply — this must NOT create a new filesystem
    output = machine.succeed(apply())
    print(f"Apply output:\n{output}")

    # Key assertion 1: must NOT have run mkfs (bootstrap path)
    assert "bootstrap" not in output.lower(), (
        f"Apply incorrectly treated existing-but-unmounted pool as bootstrap!\n{output}"
    )

    # Verify pool is mounted and accessible
    machine.succeed("mountpoint -q /mnt/storage")

    # Key assertion 2: sentinel data must be intact
    content = machine.succeed("cat /mnt/storage/sentinel.txt").strip()
    assert content == "do not destroy me", (
        f"Sentinel data destroyed — mkfs likely wiped filesystem! Got: '{content}'"
    )

    # Key assertion 3: filesystem UUID unchanged (mkfs would create a new one)
    fi_show_after = machine.succeed("btrfs fi show /mnt/storage")
    uuid_match_after = re.search(r'uuid:\s+(\S+)', fi_show_after)
    assert uuid_match_after, f"Could not find UUID after apply:\n{fi_show_after}"
    assert uuid_match_after.group(1) == original_uuid, (
        f"Filesystem UUID changed from {original_uuid} to {uuid_match_after.group(1)} — "
        f"mkfs likely destroyed and recreated filesystem!"
    )

with subtest("Resume rejects unrecoverable missing mapper target"):
    # Optional VM-level hardening guard: if checkpoint points to a mapper target
    # that cannot be recovered by pending OPEN_LUKS, resume must fail.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))

    import hashlib
    raw_cfg = machine.succeed("cat /tmp/braid-config.json")
    cfg_hash = "sha256:" + hashlib.sha256(raw_cfg.encode()).hexdigest()

    fake_checkpoint = json.dumps({
        "plan_id": "fake-missing-mapper",
        "mount_point": "/mnt/storage",
        "status": "applicable",
        "config_hash": cfg_hash,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "last_completed_action_id": "",
        "is_bootstrap": False,
        "actions": [
            {
                "id": "a1",
                "type": "ADD_DISK_BTRFS_ADD",
                "target": "/dev/mapper/does-not-exist",
                "preconditions": [],
                "status": "pending",
            }
        ],
        "warnings": [],
        "confirmations": [],
    })
    escaped = fake_checkpoint.replace("'", "'\\''")
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed(f"echo '{escaped}' > /var/lib/braid/apply-state.json")
    machine.fail(apply("--resume"))
    machine.succeed("rm -f /var/lib/braid/apply-state.json")

# --- Subtest: Failed action records error in checkpoint and history ---

with subtest("Failed action records error in checkpoint and history"):
    # Pool: disk1 + disk2 + disk3. disk4 has LUKS, not in pool.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    cmd = (
        f"printf '%s\\n' {shlex.quote(passphrase)} | "
        f"BRAID_TEST_FAIL_DURING_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json --passphrase-stdin"
    )
    machine.fail(cmd)

    # Checkpoint: failed action has "status":"failed" + "error"
    checkpoint = json.loads(machine.succeed("cat /var/lib/braid/apply-state.json"))
    failed = [a for a in checkpoint["actions"] if a["status"] == "failed"]
    assert len(failed) == 1, f"Expected 1 failed action, got {len(failed)}: {checkpoint['actions']}"
    assert failed[0]["error"] == "simulated failure via BRAID_TEST_FAIL_DURING_ACTION", (
        f"Unexpected error: {failed[0]['error']}"
    )

    # Non-failed actions: no "error" key in JSON
    for a in checkpoint["actions"]:
        if a["status"] != "failed":
            assert "error" not in a, f"Non-failed action has error key: {a}"

    # Failed history entry exists
    plan_id = checkpoint["plan_id"]
    failed_hist = json.loads(machine.succeed(
        f"cat /var/lib/braid/history/{plan_id}-failed.json"
    ))
    assert failed_hist["run_outcome"] == "failed", (
        f"Expected run_outcome=failed: {failed_hist['run_outcome']}"
    )
    assert failed_hist["failed_action_id"] == failed[0]["id"], (
        f"Expected failed_action_id={failed[0]['id']}: {failed_hist['failed_action_id']}"
    )

    # Resume: Failed{error} → InProgress → Completed (error cleared)
    machine.succeed(apply("--resume"))
    machine.fail("test -f /var/lib/braid/apply-state.json")

    # Completed history: all actions completed, no error fields
    completed_hist = json.loads(machine.succeed(
        f"cat /var/lib/braid/history/{plan_id}.json"
    ))
    assert completed_hist["run_outcome"] == "completed", (
        f"Expected run_outcome=completed: {completed_hist['run_outcome']}"
    )
    for a in completed_hist["actions"]:
        assert a["status"] == "completed", f"Expected completed: {a}"
        assert "error" not in a, f"Completed action has error key: {a}"

# --- Subtest: Reboot scenario — all LUKS closed, fresh apply re-opens and mounts ---

with subtest("Setup: ensure clean 3-disk RAID1 for reboot test"):
    machine.succeed("rm -f /var/lib/braid/apply-state.json")
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk1" in fi_show
    assert "virtio-disk2" in fi_show
    assert "virtio-disk3" in fi_show
    machine.succeed("echo 'reboot canary' > /mnt/storage/reboot-test.txt && sync")

with subtest("Reboot: LUKS closed, fresh apply re-opens and mounts"):
    # Simulate reboot: unmount + close all mappers
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk1")
    machine.succeed("cryptsetup close virtio-disk2")
    machine.succeed("cryptsetup close virtio-disk3")
    machine.succeed("rm -f /var/lib/braid/apply-state.json")

    # Fresh apply — pre-phase should open LUKS, scan, mount
    output = machine.succeed(apply())
    print(f"Reboot apply output:\n{output}")

    # Primary assertions
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/reboot-test.txt").strip()
    assert content == "reboot canary", f"Data lost after reboot: '{content}'"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2", "virtio-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    # Secondary assertion: no mkfs ran
    assert "mkfs" not in output.lower(), f"mkfs should not run on reboot:\n{output}"
    assert "bootstrap" not in output.lower(), f"bootstrap should not trigger:\n{output}"

    # Second apply should be no-op
    output2 = machine.succeed(apply())
    assert "nothing to do" in output2.lower() or "no actions" in output2.lower(), (
        f"Expected no-op after reboot apply:\n{output2}"
    )

# --- Subtest: Blocked-after-prephase — pre-phase runs even when plan is blocked ---

with subtest("Blocked after prephase: LUKS opens but plan blocks"):
    # Start from mounted 3-disk pool
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk1")
    machine.succeed("cryptsetup close virtio-disk2")
    machine.succeed("cryptsetup close virtio-disk3")
    machine.succeed("rm -f /var/lib/braid/apply-state.json")

    # Remove disk3 from config (triggers identity ambiguity → blocked)
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk99",
    ]))

    # Apply should exit 1 (blocked)
    machine.fail(apply())

    # Assert: pre-phase ran — LUKS mappers are open
    machine.succeed("test -e /dev/mapper/virtio-disk1")
    machine.succeed("test -e /dev/mapper/virtio-disk2")

    # Assert: pool is mounted (pre-phase mounted it)
    machine.succeed("mountpoint -q /mnt/storage")

    # Assert: data intact
    content = machine.succeed("cat /mnt/storage/reboot-test.txt").strip()
    assert content == "reboot canary", f"Data lost: '{content}'"

# --- Subtest: Resume with stale checkpoint after reboot → re-plans ---

with subtest("Resume with stale checkpoint after reboot replans"):
    # Restore config to 3-disk
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # Ensure clean state
    machine.succeed("rm -f /var/lib/braid/apply-state.json")
    machine.succeed(apply())

    # Create a checkpoint, then simulate reboot
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk4", extra="--force", confirm="reformat this disk"))
    cmd = (
        f"printf '%s\\n' {shlex.quote(passphrase)} | "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json --passphrase-stdin"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Simulate reboot: unmount + close all mappers
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk1")
    machine.succeed("cryptsetup close virtio-disk2")
    machine.succeed("cryptsetup close virtio-disk3")
    machine.succeed("cryptsetup close virtio-disk4 || true")

    # Resume should detect stale checkpoint and re-plan
    output = machine.succeed(apply("--resume"))
    print(f"Resume-after-reboot output:\n{output}")

    # Checkpoint should be cleaned up
    machine.fail("test -f /var/lib/braid/apply-state.json")

    # Pool should be healthy with all 4 disks
    machine.succeed("mountpoint -q /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 not in pool after resume-replan:\n{fi_show}"

machine.shutdown()
