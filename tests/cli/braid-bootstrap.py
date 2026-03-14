import json

# Test: braid bootstrap (first-disk via init-disk + plan/apply)
#
# Validates the full day-one workflow using the unified CLI:
#   1. No pool exists → init-disk → plan shows OPEN_LUKS + ADD → apply → pool created
#   2. Pool healthy with 1 disk
#   3. init-disk second disk → plan → apply → RAID1
#   4. Data integrity throughout

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def write_config(disk_list, mount="/mnt/storage"):
    config = json.dumps({"disks": disk_list, "mountPoint": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def plan(extra=""):
    return f"braid plan --config /tmp/braid-config.json {extra}"


def plan_json():
    raw = machine.succeed(plan("--json"))
    return json.loads(raw)


def apply(extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid apply --config /tmp/braid-config.json --passphrase-stdin {extra}"
    )


def init_disk(by_id):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk --config /tmp/braid-config.json --passphrase-stdin {by_id}"
    )


# --- Phase 1: Plan with no pool (disk not yet init'd => blocked) ---

with subtest("Plan warns when disk not LUKS formatted"):
    machine.succeed(write_config(["/dev/disk/by-id/virtio-disk1"]))
    p = plan_json()
    # Plan is applicable (no blocked_reasons) but warns about non-LUKS disk
    assert p["status"] == "applicable", (
        f"Expected applicable status:\n{p}"
    )
    assert any(w["code"] == "INIT_REQUIRED" for w in p["warnings"]), (
        f"Expected INIT_REQUIRED warning:\n{p['warnings']}"
    )

# --- Phase 2: init-disk, then plan shows OPEN_LUKS ---

with subtest("After init-disk, plan produces OPEN_LUKS + ADD actions"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"
    # Single disk — no RAID1 balance
    assert "BALANCE_TO_RAID1" not in types, f"Unexpected BALANCE_TO_RAID1:\n{types}"
    assert p["status"] == "applicable", f"Expected applicable status:\n{p}"


# --- Phase 3: Apply creates pool from scratch ---

with subtest("Apply creates single-disk pool"):
    machine.succeed(apply())
    machine.succeed("mountpoint -q /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/virtio-disk1" in fi_show, f"disk1 not in pool:\n{fi_show}"


# --- Phase 4: Status works on new pool ---

with subtest("Status reports intact single-disk pool"):
    output = machine.succeed("braid status --config /tmp/braid-config.json")
    assert "intact" in output.lower(), f"Expected intact status:\n{output}"


# --- Phase 5: Write data for integrity check ---

with subtest("Write test data"):
    machine.succeed("echo 'bootstrap test data' > /mnt/storage/test.txt && sync")
    content = machine.succeed("cat /mnt/storage/test.txt")
    assert "bootstrap test data" in content, f"Data mismatch:\n{content}"


# --- Phase 6: Plan to add second disk ---

with subtest("Plan shows OPEN_LUKS + ADD + RAID1 balance for second disk"):
    machine.succeed(write_config([
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]))
    # init-disk disk2 first
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    p = plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"
    assert "BALANCE_TO_RAID1" in types, f"Missing BALANCE_TO_RAID1:\n{types}"

    # Target should be disk2
    open_action = [a for a in p["actions"] if a["type"] == "OPEN_LUKS"][0]
    assert "virtio-disk2" in open_action["target"], f"Wrong target:\n{open_action}"


# --- Phase 7: Apply adds second disk and converts to RAID1 ---

with subtest("Apply adds second disk and converts to RAID1"):
    machine.succeed(apply())
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/virtio-disk1" in fi_show, f"disk1 not in pool:\n{fi_show}"
    assert "/dev/mapper/virtio-disk2" in fi_show, f"disk2 not in pool:\n{fi_show}"

    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in fi_df, f"Expected RAID1 profile:\n{fi_df}"


# --- Phase 8: Data integrity after RAID1 conversion ---

with subtest("Data intact after RAID1 conversion"):
    content = machine.succeed("cat /mnt/storage/test.txt")
    assert "bootstrap test data" in content, f"Data lost after RAID1:\n{content}"


# --- Phase 9: No-op plan after convergence ---

with subtest("No-op plan after full convergence"):
    p = plan_json()
    mutation_actions = [a for a in p["actions"] if not a["type"].startswith("VERIFY_")]
    assert len(mutation_actions) == 0, f"Expected zero mutation actions:\n{p['actions']}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"


machine.shutdown()
