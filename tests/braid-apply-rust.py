import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def init_disk(by_id, extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid init-disk --config /tmp/braid-config.json {by_id} {extra}"


def rust_apply(extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid-rust apply --config /tmp/braid-config.json {extra}"


def bash_apply(extra="", confirm=""):
    env = f"BRAID_PASSPHRASE='{passphrase}' BRAID_LUKS_OPTS='{luks_opts}'"
    if confirm:
        env += f" BRAID_CONFIRM='{confirm}'"
    return f"{env} braid apply --config /tmp/braid-config.json {extra}"


# --- Subtest 0: Setup — init 2 disks via bash, build pool ---

with subtest("Setup: init 2 disks via bash"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))

# --- Subtest 1: Fresh apply builds 2-disk RAID1 ---

with subtest("Fresh apply builds 2-disk RAID1"):
    output = machine.succeed(rust_apply())
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
    output = machine.succeed(rust_apply())
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
    output = machine.succeed(rust_apply())
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
    machine.succeed(rust_apply())

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" not in fi_show, f"disk3 still in pool:\n{fi_show}"
    machine.fail("test -e /dev/mapper/virtio-disk3")

# --- Subtest 8: Redundancy refusal without confirmation ---

with subtest("Redundancy refusal without confirmation"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    machine.fail(rust_apply())

# --- Subtest 9: Redundancy acceptance with phrase ---

with subtest("Redundancy acceptance with correct phrase"):
    machine.succeed(rust_apply(confirm="remove this disk without redundancy"))
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
    output = machine.succeed(rust_apply())
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
    machine.succeed(rust_apply(
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
    machine.succeed(rust_apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk4" in fi_show, f"disk4 not in pool:\n{fi_show}"

with subtest("Blocked plan exits 1"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk99",
    ]))
    machine.fail(rust_apply())

# --- Subtest 13: --allow-remove-ambiguous with confirmation ---

with subtest("--allow-remove-ambiguous with confirmation"):
    output = machine.succeed(rust_apply(
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
    output = machine.succeed(rust_apply(
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
    machine.succeed(rust_apply())
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
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid-rust apply --config /tmp/braid-config.json"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    checkpoint = json.loads(machine.succeed("cat /var/lib/braid/apply-state.json"))
    completed = [a for a in checkpoint["actions"] if a["status"] == "completed"]
    assert len(completed) >= 1, f"Expected at least 1 completed action:\n{checkpoint}"

# --- Subtest 16: Resume continues from checkpoint ---

with subtest("Resume continues from checkpoint"):
    machine.succeed(rust_apply("--resume"))
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "virtio-disk3" in fi_show, f"disk3 not in pool after resume:\n{fi_show}"
    machine.fail("test -f /var/lib/braid/apply-state.json")

# --- Subtest 17: Stale checkpoint refuses resume ---

with subtest("Stale checkpoint refuses resume"):
    fake_checkpoint = json.dumps({
        "schema_version": 1,
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
    machine.fail(rust_apply("--resume"))
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
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_TEST_FAIL_AFTER_ACTION=a1 "
        f"braid-rust apply --config /tmp/braid-config.json"
    )
    machine.fail(cmd)
    machine.succeed("test -f /var/lib/braid/apply-state.json")

    # Hide disk4 to simulate absence
    machine.succeed("cryptsetup close virtio-disk4 || true")
    machine.succeed("mv /dev/disk/by-id/virtio-disk4 /dev/disk/by-id/virtio-disk4.hidden")

    # Resume should fail
    machine.fail(rust_apply("--resume"))
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
    raw = machine.succeed("braid-rust plan --config /tmp/braid-config.json --json")
    p = json.loads(raw)
    for a in p["actions"]:
        assert "FORMAT" not in a["type"], (
            f"ActionType contains FORMAT — safety invariant violated: {a}"
        )

machine.shutdown()
