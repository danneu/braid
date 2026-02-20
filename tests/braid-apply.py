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

# --- Phase 2: Apply add-disk ---

with subtest("Apply adds disk3 to pool"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]))
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

# --- Phase 4: Apply replace (add + missing-remove) ---

with subtest("Setup: simulate disk2 death"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk2")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")

with subtest("Apply replaces dead disk2 with disk3"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(apply())

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

# --- Phase 6: Interrupted apply + resume ---

with subtest("Setup: rebuild 2-disk pool for resume test"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
    ]))
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "RAID1" in machine.succeed("btrfs fi df /mnt/storage")

with subtest("Interrupted apply leaves checkpoint"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk3",
        "/dev/disk/by-id/virtio-disk4",
    ]))
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

machine.shutdown()
