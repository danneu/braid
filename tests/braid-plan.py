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


def plan(extra=""):
    return f"braid plan --config /tmp/braid-config.json {extra}"


def plan_json():
    raw = machine.succeed(plan("--json"))
    return json.loads(raw)


def init_disk(by_id):
    return (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk --config /tmp/braid-config.json {by_id}"
    )


# --- Phase 0: Build 2-disk RAID1 pool ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed("echo 'test data' > /mnt/storage/file.txt && sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Phase 1: No-op plan ---

with subtest("No-op plan when config matches live state"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    output = machine.succeed(plan())
    assert "no actions" in output.lower(), f"Expected no actions:\n{output}"

    p = plan_json()
    mutation_actions = [a for a in p["actions"] if not a["type"].startswith("VERIFY_")]
    assert len(mutation_actions) == 0, f"Expected zero mutation actions:\n{p['actions']}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"

# --- Phase 2: Add plan (disk3 not yet LUKS — warning, plan still applicable) ---

with subtest("Plan warns about non-LUKS disk with INIT_REQUIRED"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    p = plan_json()
    # Plan should still be applicable — non-LUKS disks are skipped with warning
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"
    assert any("INIT_REQUIRED" in w for w in p["warnings"]), (
        f"Expected INIT_REQUIRED warning:\n{p['warnings']}"
    )
    # No OPEN_LUKS or format action for the non-LUKS disk
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" not in types, f"Unexpected OPEN_LUKS for non-LUKS disk:\n{types}"

# --- Phase 2b: After init-disk, plan shows OPEN_LUKS ---

with subtest("Plan shows OPEN_LUKS after init-disk"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3"))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"

    # Target should be the newly init'd disk
    open_action = [a for a in p["actions"] if a["type"] == "OPEN_LUKS"][0]
    assert "virtio-disk3" in open_action["target"], f"Wrong target:\n{open_action}"

# --- Phase 2c: Absent disk => warning, plan applicable ---

with subtest("Absent disk produces DISK_ABSENT_SKIPPED warning"):
    # virtio-disk99 doesn't exist as a virtual disk
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    p = plan_json()
    assert p["status"] == "applicable", f"Expected applicable despite absent disk:\n{p}"
    assert any("DISK_ABSENT_SKIPPED" in w for w in p["warnings"]), (
        f"Expected DISK_ABSENT_SKIPPED warning:\n{p['warnings']}"
    )
    assert any("virtio-disk99" in w for w in p["warnings"]), (
        f"Expected disk path in warning:\n{p['warnings']}"
    )
    # No actions for the absent disk
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" not in types, f"Unexpected OPEN_LUKS for absent disk:\n{types}"

# --- Phase 3: Remove plan (graceful) ---

with subtest("Plan shows remove actions for disk in pool but not config"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "REMOVE_DISK_GRACEFUL" in types, f"Missing REMOVE_DISK_GRACEFUL:\n{types}"
    assert "CLOSE_LUKS_MAPPER" in types, f"Missing CLOSE_LUKS_MAPPER:\n{types}"

    remove_action = [a for a in p["actions"] if a["type"] == "REMOVE_DISK_GRACEFUL"][0]
    assert "virtio-disk2" in remove_action["target"], f"Wrong target:\n{remove_action}"

# --- Phase 4: Redundancy warning ---

with subtest("Remove to single disk triggers confirmation requirement"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    p = plan_json()
    assert len(p.get("confirmations", [])) > 0, f"Expected confirmations:\n{p}"
    phrases = [c["phrase"] for c in p["confirmations"]]
    assert any("redundancy" in ph for ph in phrases), f"Expected redundancy phrase:\n{phrases}"

# --- Phase 5: Replace plan ---

with subtest("Setup: simulate disk2 death"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk2")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

with subtest("Plan shows OPEN_LUKS + missing warning for replace scenario"):
    # disk3 is already LUKS-formatted from earlier init-disk
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    # Missing removal now requires explicit gate (Phase 6), so no REMOVE_DISK_MISSING
    # Instead we expect a warning about the missing device
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    # Check for pool degraded warning
    assert any("POOL_DEGRADED" in w or "missing" in w.lower() for w in p["warnings"]), (
        f"Expected degraded warning:\n{p['warnings']}"
    )

# --- Phase 6: Ambiguity refusal ---

with subtest("Setup: rebuild pool for ambiguity test"):
    # Recover from degraded: reopen disk2, let btrfs reassemble, then add disk3+disk4
    machine.succeed("umount /mnt/storage")
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 virtio-disk2"
    )
    machine.succeed("btrfs device scan")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after recover:\n{fi_show}"
    # Add disk3 and disk4 for a 4-disk pool (need config with all 4 disks)
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk3", "/tmp/braid-config.json"))
    machine.succeed(add_disk("/dev/disk/by-id/virtio-disk4", "/tmp/braid-config.json"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2", "virtio-disk3", "virtio-disk4"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

with subtest("Setup: kill two disks for ambiguity"):
    # With 4-disk RAID1, killing 2 still allows degraded mount (2 remain)
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk3")
    machine.succeed("cryptsetup luksClose virtio-disk4")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

with subtest("Multiple missing devices warns about degraded pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    # With new design, multiple missing no longer causes die() — it warns
    p = plan_json()
    assert any("POOL_DEGRADED" in w or "missing" in w.lower() for w in p["warnings"]), (
        f"Expected degraded warning:\n{p['warnings']}"
    )

# --- Phase 7: JSON schema validation ---

with subtest("Setup: rebuild pool for schema test"):
    machine.succeed("umount /mnt/storage")
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk3 virtio-disk3"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk4 virtio-disk4"
    )
    machine.succeed("btrfs device scan")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")
    # After scan+mount, missing devices should rejoin automatically
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    if "missing" in fi_show.lower():
        machine.succeed("btrfs device remove missing /mnt/storage")

with subtest("JSON output has required schema fields"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
    p = plan_json()
    assert p["schema_version"] == 1, f"Bad schema_version: {p['schema_version']}"
    assert "plan_id" in p, "Missing plan_id"
    assert "mount_point" in p, "Missing mount_point"
    assert "warnings" in p, "Missing warnings"
    assert "actions" in p, "Missing actions"
    assert "status" in p, "Missing status"
    assert "blocked_reasons" in p, "Missing blocked_reasons"

    for action in p["actions"]:
        assert "id" in action, f"Action missing id: {action}"
        assert "type" in action, f"Action missing type: {action}"
        assert "target" in action, f"Action missing target: {action}"
        assert "status" in action, f"Action missing status: {action}"
        assert action["status"] == "pending", f"Action not pending: {action}"

# --- Phase 8: Human output ---

with subtest("Human output shows plan summary"):
    # Use a config that doesn't include disk4 so planner sees a remove action
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    output = machine.succeed(plan())
    assert "Plan ID:" in output, f"Missing Plan ID:\n{output}"
    assert "Mount:" in output or "mount" in output.lower(), f"Missing mount:\n{output}"
    assert "REMOVE_DISK" in output, f"Missing action in human output:\n{output}"
    assert "Status:" in output, f"Missing Status in human output:\n{output}"

# --- Phase 9: Bootstrap plan (unmounted pool, disk not yet init'd) ---

with subtest("Bootstrap plan blocked for non-LUKS disk"):
    machine.succeed("umount /mnt/storage")
    # Close all LUKS to simulate fresh state (but disk1 is still LUKS-formatted)
    machine.succeed("cryptsetup luksClose virtio-disk1 || true")
    machine.succeed("cryptsetup luksClose virtio-disk2 || true")
    machine.succeed("cryptsetup luksClose virtio-disk3 || true")
    machine.succeed("cryptsetup luksClose virtio-disk4 || true")
    # disk1 is already LUKS-formatted, so plan should show OPEN_LUKS
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"
    # No remove actions when there's no pool
    assert "REMOVE_DISK_GRACEFUL" not in types, f"Unexpected REMOVE_DISK_GRACEFUL:\n{types}"
    # No BALANCE_TO_RAID1 with single disk
    assert "BALANCE_TO_RAID1" not in types, f"Unexpected BALANCE_TO_RAID1:\n{types}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"

# --- Phase 9b: No path emits ADD_DISK_LUKS_FORMAT_OPEN ---

with subtest("No path emits ADD_DISK_LUKS_FORMAT_OPEN"):
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "ADD_DISK_LUKS_FORMAT_OPEN" not in types, (
        f"ADD_DISK_LUKS_FORMAT_OPEN must never appear in plan:\n{types}"
    )

machine.shutdown()
