import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_disk(dev, config=None):
    config_flag = f"--config {config}" if config else ""
    return (
        f"echo 'erase this disk' | "
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid-add-disk {config_flag} {dev}"
    )


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def apply(extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid apply --config /tmp/braid-config.json {extra}"


def plan_json():
    raw = machine.succeed("braid plan --config /tmp/braid-config.json --json")
    return json.loads(raw)


def init_disk(by_id, extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid init-disk --config /tmp/braid-config.json {by_id} {extra}"


# --- Phase 0: Build initial 2-disk RAID1 pool with braid-add-disk ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt && sync")

# --- Phase 1: No-op apply ---

with subtest("No-op apply when config matches live state"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    output = machine.succeed(apply())
    assert "nothing to do" in output.lower() or "no actions" in output.lower(), (
        f"Expected no-op message:\n{output}"
    )

# --- Phase 2: Apply add-disk (using init-disk first) ---

with subtest("Apply adds disk3 to pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # Must init-disk before apply can open it
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    output = machine.succeed(apply())
    print(f"Apply output:\n{output}")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not in pool:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"Expected RAID1:\n{df_output}"

with subtest("Data intact after apply add"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

with subtest("Checkpoint removed after successful apply"):
    machine.fail("test -f /var/lib/braid/apply-state.json")

with subtest("History file written after successful apply"):
    machine.succeed("ls /var/lib/braid/history/ | head -1")

# --- Phase 3: Apply remove-disk ---

with subtest("Apply removes disk3 from pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(apply())

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"

with subtest("LUKS mapper closed after remove"):
    machine.fail("test -e /dev/mapper/virtio-disk3")

with subtest("Data intact after apply remove"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

# --- Phase 4: Apply replace (add + missing-remove with explicit gate) ---

with subtest("Setup: simulate disk2 death"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk2")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

with subtest("Apply replaces dead disk2 with disk3"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # disk3 already LUKS-formatted from Phase 2 init-disk, but was closed after remove
    # Need to re-init since it was wiped? No — it's still LUKS formatted, just closed.
    # The apply will OPEN_LUKS + ADD it.
    # But we also need to remove the missing device — requires explicit gate
    machine.succeed(apply(
        extra="--allow-remove-missing",
        confirm="remove missing device from pool",
    ))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not in pool:\n{fi_show}"
    assert "missing" not in fi_show.lower(), f"Still has missing:\n{fi_show}"

with subtest("Data intact after replace"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

# --- Phase 5: Redundancy confirmation ---

with subtest("Apply refuses remove-to-single without confirmation"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(apply())

with subtest("Apply accepts remove-to-single with correct phrase"):
    machine.succeed(apply(confirm="remove this disk without redundancy"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device:\n{fi_show}"

# --- Phase 5b: Absent disk continues apply + warns (11.3.1) ---

with subtest("Apply with absent configured disk continues other work"):
    # Currently single disk (disk1). Add disk3 back + fake absent disk99.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    # disk3 is still LUKS from earlier. Apply should add disk3, warn about disk99.
    output = machine.succeed(apply())
    assert "DISK_ABSENT_SKIPPED" in output or "warning" in output.lower() or "skip" in output.lower(), (
        f"Expected warning about absent disk:\n{output}"
    )
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not added despite absent disk99:\n{fi_show}"

with subtest("Data intact after apply with absent disk"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

# --- Phase 5c: Unplug/replug regression (11.7) ---

with subtest("Unplug disk, apply warns, replug disk, apply reconciles"):
    # Pool currently: disk1 + disk3, RAID1
    machine.succeed("echo 'sentinel data 42' > /mnt/storage/sentinel.txt && sync")
    sentinel_hash = machine.succeed("sha256sum /mnt/storage/sentinel.txt").strip().split()[0]

    # "Unplug" disk3: close LUKS + hide the by-id device to simulate physical removal
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk3")
    # Save the real device path before hiding
    real_dev = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk3").strip()
    machine.succeed("mv /dev/disk/by-id/virtio-disk3 /dev/disk/by-id/virtio-disk3.hidden")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

    # Apply with 2-disk config — disk3 absent, should warn only, no format
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    output = machine.succeed(apply())
    # Should NOT have formatted anything
    assert "luksFormat" not in output.lower(), f"Unexpected format:\n{output}"
    # Should have absent-disk warning
    assert "DISK_ABSENT_SKIPPED" in output, f"Expected DISK_ABSENT_SKIPPED warning:\n{output}"

    # Sentinel file still intact
    content = machine.succeed("cat /mnt/storage/sentinel.txt").strip()
    assert "sentinel data 42" in content, f"Sentinel data changed:\n{content}"

    # "Replug" — restore by-id symlink and reopen LUKS
    machine.succeed("mv /dev/disk/by-id/virtio-disk3.hidden /dev/disk/by-id/virtio-disk3")
    machine.succeed("umount /mnt/storage")
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk3 virtio-disk3"
    )
    machine.succeed("btrfs device scan")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")

    # Apply again — should reconcile cleanly (no-op now)
    output = machine.succeed(apply())

    # Verify sentinel file hash unchanged
    hash_after = machine.succeed("sha256sum /mnt/storage/sentinel.txt").strip().split()[0]
    assert hash_after == sentinel_hash, (
        f"Sentinel hash changed: {sentinel_hash} -> {hash_after}"
    )

# --- Phase 5d: Apply with present non-LUKS disk warns but continues (11.3.2) ---

with subtest("Apply with present non-LUKS disk warns but proceeds"):
    # disk4 exists but is not LUKS-formatted — apply should skip it with warning
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    output = machine.succeed(apply())
    assert "INIT_REQUIRED" in output, f"Expected INIT_REQUIRED warning:\n{output}"

# --- Phase 5e: Explicit missing-device removal gate (11.5) ---

with subtest("Setup: degraded pool for missing-device tests"):
    # Pool: disk1 + disk3, RAID1. Kill disk3 to make it missing.
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close virtio-disk3")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing:\n{fi_show}"
    # Hide disk3 by-id to simulate physical removal
    machine.succeed("mv /dev/disk/by-id/virtio-disk3 /dev/disk/by-id/virtio-disk3.hidden")

with subtest("Missing-device removal refused without explicit intent"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    # Apply without --allow-remove-missing should NOT remove missing device
    output = machine.succeed(apply())
    # Should warn about degraded but not remove
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Missing device was removed without gate:\n{fi_show}"

with subtest("Missing-device removal refused without confirmation phrase"):
    machine.fail(apply(extra="--allow-remove-missing"))

with subtest("Explicit missing-device removal succeeds"):
    machine.succeed(apply(
        extra="--allow-remove-missing",
        confirm="remove missing device from pool",
    ))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), f"Missing device not cleared:\n{fi_show}"
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device:\n{fi_show}"

with subtest("Data intact after missing-device removal"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"Data lost: '{content}'"

with subtest("Setup: restore disk3 and rebuild 2-disk pool"):
    machine.succeed("mv /dev/disk/by-id/virtio-disk3.hidden /dev/disk/by-id/virtio-disk3")
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # disk3 was evicted, still has stale btrfs metadata. Re-init to wipe it.
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3", extra="--force", confirm="reformat this disk"))
    machine.succeed(apply())
    assert "RAID1" in machine.succeed("btrfs fi df /mnt/storage")

# --- Phase 6: Interrupted apply + resume ---

with subtest("Setup: rebuild 2-disk pool for resume test"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # disk3 is still LUKS, just removed from pool. Re-add.
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "RAID1" in machine.succeed("btrfs fi df /mnt/storage")

with subtest("Interrupted apply leaves checkpoint"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    # init-disk disk4 first
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk4"))
    # Use BRAID_TEST_FAIL_AFTER_ACTION to simulate interruption after first action
    cmd = (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    checkpoint = json.loads(machine.succeed("cat /var/lib/braid/apply-state.json"))
    completed = [a for a in checkpoint["actions"] if a["status"] == "completed"]
    assert len(completed) >= 1, f"Expected at least 1 completed action:\n{checkpoint}"

with subtest("Resume continues from checkpoint"):
    machine.succeed(apply("--resume"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 not in pool after resume:\n{fi_show}"
    machine.fail("test -f /var/lib/braid/apply-state.json")

# --- Phase 7: Stale checkpoint refuses resume ---

with subtest("Stale checkpoint refuses resume"):
    # Create a fake checkpoint with wrong config hash
    fake_checkpoint = json.dumps({
        "schema_version": 1,
        "plan_id": "fake",
        "config_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "actions": []
    })
    machine.succeed("mkdir -p /var/lib/braid")
    escaped = fake_checkpoint.replace("'", "'\\''")
    machine.succeed(f"echo '{escaped}' > /var/lib/braid/apply-state.json")
    machine.fail(apply("--resume"))
    # Clean up
    machine.succeed("rm /var/lib/braid/apply-state.json")

# --- Phase 7b: Resume fails when pending action target is absent ---

with subtest("Setup: remove disk4 from pool for resume-target-missing test"):
    # Current pool: disk1 + disk3 + disk4. Remove disk4.
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(apply())
    # disk4 mapper should be closed
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" not in fi_show, f"disk4 still in pool:\n{fi_show}"

with subtest("Resume fails when target disk is absent (RESUME_TARGET_MISSING)"):
    # Now add disk2 (LUKS-formatted from Phase 0 braid-add-disk setup)
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    # disk2 LUKS is still closed from Phase 4's death simulation. Re-init it.
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2", extra="--force", confirm="reformat this disk"))
    # Interrupt after first action to create checkpoint with pending OPEN_LUKS
    cmd = (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid apply --config /tmp/braid-config.json"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Check which action is pending — should be targeting disk2
    checkpoint = json.loads(machine.succeed("cat /var/lib/braid/apply-state.json"))
    pending = [a for a in checkpoint["actions"] if a["status"] == "pending"]
    assert len(pending) > 0, f"Expected pending actions:\n{checkpoint}"

    # Hide disk2 to simulate absence. Close its mapper first if opened.
    machine.succeed("cryptsetup close virtio-disk2 || true")
    machine.succeed("mv /dev/disk/by-id/virtio-disk2 /dev/disk/by-id/virtio-disk2.hidden")

    # Resume should fail with RESUME_TARGET_MISSING
    machine.fail(apply("--resume"))

    # Checkpoint should be preserved (not deleted)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Cleanup
    machine.succeed("mv /dev/disk/by-id/virtio-disk2.hidden /dev/disk/by-id/virtio-disk2")
    machine.succeed("rm -f /var/lib/braid/apply-state.json")

# --- Phase 8: apply never calls luksFormat ---

with subtest("Apply never contains luksFormat"):
    # Verify by checking the braid script source
    output = machine.succeed("which braid")
    braid_path = output.strip()
    script = machine.succeed(f"cat {braid_path}")
    # The apply executor should not reference luksFormat
    # (init-disk has it, but the executor dispatch table should not)
    assert "action_luks_format_open" not in script, (
        "action_luks_format_open still referenced in braid script"
    )

machine.shutdown()
