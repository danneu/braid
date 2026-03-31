import json

start_all()
machine.wait_for_unit("multi-user.target")

import shlex

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def init_disk(dev, force=False, config=None):
    passphrase_q = shlex.quote(passphrase)
    force_flag = "--force" if force else ""
    confirm_env = "BRAID_CONFIRM='reformat this disk' " if force else ""
    config_flag = f"--config {config}" if config else ""
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"{confirm_env}"
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid init-disk --passphrase-stdin {force_flag} {config_flag} {dev}"
    )


def apply_cmd(config=None, extra="", confirm=""):
    passphrase_q = shlex.quote(passphrase)
    config_flag = f"--config {config}" if config else ""
    env = ""
    if confirm:
        env = f"BRAID_CONFIRM='{confirm}' "
    return f"printf '%s\\n' {passphrase_q} | {env}braid apply --passphrase-stdin {config_flag} {extra}"


def write_config(mount="/mnt/storage"):
    config = json.dumps({"mount_point": mount})
    escaped = config.replace("'", "'\\''")
    return f"echo '{escaped}' > /tmp/braid-config.json"


def rust_plan(extra=""):
    return f"braid plan --config /tmp/braid-config.json {extra}"


def rust_plan_json():
    raw = machine.succeed(rust_plan("--json"))
    return json.loads(raw)


# --- Subtest 0: Setup — build 2-disk RAID1 pool using bash braid ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk1"))
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk2"))
    machine.succeed(apply_cmd())
    machine.succeed("echo 'test data' > /mnt/storage/file.txt && sync")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["virtio-disk1", "virtio-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

# --- Subtest 1: No-op plan ---

with subtest("No-op plan when config matches live state"):
    machine.succeed(write_config())
    p = rust_plan_json()
    mutation_actions = [a for a in p["actions"] if not a["type"].startswith("VERIFY_")]
    assert len(mutation_actions) == 0, f"Expected zero mutation actions:\n{p['actions']}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"

# --- Subtest 2: Non-LUKS disk warning ---

with subtest("Plan warns about non-LUKS disk with INIT_REQUIRED"):
    machine.succeed(write_config())
    p = rust_plan_json()
    warning_codes = [w["code"] for w in p["warnings"]]
    assert "INIT_REQUIRED" in warning_codes, (
        f"Expected INIT_REQUIRED warning:\n{p['warnings']}"
    )
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" not in types, f"Unexpected OPEN_LUKS for non-LUKS disk:\n{types}"

# --- Subtest 3: After init-disk → OPEN_LUKS + ADD ---

with subtest("Plan shows OPEN_LUKS after init-disk"):
    machine.succeed(init_disk("/dev/disk/by-id/virtio-disk3", config="/tmp/braid-config.json"))
    p = rust_plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"

    open_action = [a for a in p["actions"] if a["type"] == "OPEN_LUKS"][0]
    assert "virtio-disk3" in open_action["target"], f"Wrong target:\n{open_action}"
    open_cmds = [c["command"] for c in open_action["commands"]]
    assert any("cryptsetup open" in c for c in open_cmds), (
        f"OPEN_LUKS missing cryptsetup open command:\n{open_cmds}"
    )

    add_action = [a for a in p["actions"] if a["type"] == "ADD_DISK_BTRFS_ADD"][0]
    add_cmds = [c["command"] for c in add_action["commands"]]
    assert any("btrfs device add" in c for c in add_cmds), (
        f"ADD_DISK_BTRFS_ADD missing btrfs device add command:\n{add_cmds}"
    )

# --- Subtest 4: Absent disk → DISK_ABSENT_SKIPPED ---

with subtest("Absent disk produces DISK_ABSENT_SKIPPED warning"):
    machine.succeed(write_config())
    p = rust_plan_json()
    warning_codes = [w["code"] for w in p["warnings"]]
    assert "DISK_ABSENT_SKIPPED" in warning_codes, (
        f"Expected DISK_ABSENT_SKIPPED warning:\n{p['warnings']}"
    )
    warning_msgs = [w["message"] for w in p["warnings"]]
    assert any("virtio-disk99" in m for m in warning_msgs), (
        f"Expected disk path in warning message:\n{warning_msgs}"
    )

# --- Subtest 5: Absent blocks removal ---

with subtest("Absent config disk blocks removal when pool has unmatched device"):
    machine.succeed(write_config())
    p = rust_plan_json()
    assert p["status"] == "blocked", f"Expected blocked:\n{p}"
    blocked_codes = [r["code"] for r in p["blocked_reasons"]]
    assert "IDENTITY_AMBIGUOUS_ABSENT_DISK" in blocked_codes, (
        f"Expected IDENTITY_AMBIGUOUS_ABSENT_DISK:\n{p['blocked_reasons']}"
    )

# --- Subtest 6: --allow-remove-ambiguous ---

with subtest("--allow-remove-ambiguous unblocks plan with confirmations"):
    raw = machine.succeed(rust_plan("--json --allow-remove-ambiguous"))
    p = json.loads(raw)
    assert p["status"] == "applicable", (
        f"Expected applicable with override:\n{p}"
    )
    phrases = [c["phrase"] for c in p.get("confirmations", [])]
    assert "remove despite ambiguous identity" in phrases, (
        f"Expected ambiguous identity confirmation:\n{phrases}"
    )
    assert "remove this disk without redundancy" in phrases, (
        f"Expected redundancy confirmation:\n{phrases}"
    )
    assert len(phrases) == 2, f"Expected exactly 2 confirmations:\n{phrases}"

# --- Subtest 7: Graceful remove ---

with subtest("Plan shows remove actions for disk in pool but not config"):
    machine.succeed(write_config())
    p = rust_plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "REMOVE_DISK_GRACEFUL" in types, f"Missing REMOVE_DISK_GRACEFUL:\n{types}"
    assert "CLOSE_LUKS_MAPPER" in types, f"Missing CLOSE_LUKS_MAPPER:\n{types}"

    remove_action = [a for a in p["actions"] if a["type"] == "REMOVE_DISK_GRACEFUL"][0]
    assert "virtio-disk2" in remove_action["target"], f"Wrong target:\n{remove_action}"
    # 2-disk pool removing one → should have balance-to-single + device remove
    remove_cmds = [c["command"] for c in remove_action["commands"]]
    assert any("btrfs balance start -dconvert=single" in c for c in remove_cmds), (
        f"REMOVE_DISK_GRACEFUL missing balance-to-single:\n{remove_cmds}"
    )
    assert any("btrfs device remove" in c for c in remove_cmds), (
        f"REMOVE_DISK_GRACEFUL missing btrfs device remove:\n{remove_cmds}"
    )

# --- Subtest 8: Redundancy confirmation ---

with subtest("Remove to single disk triggers redundancy confirmation"):
    p = rust_plan_json()
    phrases = [c["phrase"] for c in p.get("confirmations", [])]
    assert any("redundancy" in ph for ph in phrases), f"Expected redundancy phrase:\n{phrases}"

# --- Subtest 9: Degraded pool warning text ---

with subtest("Degraded pool warning includes --allow-remove-missing hint"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk2")
    machine.succeed("mount -o degraded /dev/mapper/virtio-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"Expected missing device:\n{fi_show}"

    machine.succeed(write_config())
    p = rust_plan_json()
    degraded_warnings = [w for w in p["warnings"] if w["code"] == "POOL_DEGRADED_MISSING_DEVICES"]
    assert len(degraded_warnings) > 0, f"Expected POOL_DEGRADED warning:\n{p['warnings']}"
    assert any("--allow-remove-missing" in w["message"] for w in degraded_warnings), (
        f"Expected --allow-remove-missing hint in degraded warning:\n{degraded_warnings}"
    )

# --- Subtest 10: JSON schema validation ---

with subtest("JSON output has required schema fields"):
    # Recover: reopen disk2, reassemble pool
    machine.succeed("umount /mnt/storage")
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 virtio-disk2"
    )
    machine.succeed("btrfs device scan")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    if "missing" in fi_show.lower():
        machine.succeed("btrfs device remove missing /mnt/storage")

    machine.succeed(write_config())
    p = rust_plan_json()
    assert "plan_id" in p, "Missing plan_id"
    assert "mount_point" in p, "Missing mount_point"
    assert "warnings" in p, "Missing warnings"
    assert "actions" in p, "Missing actions"
    assert "status" in p, "Missing status"
    assert "blocked_reasons" in p, "Missing blocked_reasons"

    assert "warning_count" in p, "Missing warning_count"
    assert isinstance(p["warning_count"], int), f"warning_count not int: {type(p['warning_count'])}"
    assert "summary" in p, "Missing summary"
    s = p["summary"]
    for f in ["actions_total", "actions_mutation", "actions_verify",
              "warnings_total", "blocked_total", "skipped_total"]:
        assert f in s, f"Missing summary.{f}"
        assert isinstance(s[f], int), f"summary.{f} not int: {type(s[f])}"
    assert s["actions_total"] == s["actions_mutation"] + s["actions_verify"], (
        f"actions_total mismatch: {s['actions_total']} != {s['actions_mutation']} + {s['actions_verify']}"
    )
    assert s["warnings_total"] == len(p["warnings"]), (
        f"warnings_total mismatch: {s['warnings_total']} != {len(p['warnings'])}"
    )
    assert s["warnings_total"] == p["warning_count"], (
        f"warnings_total != warning_count: {s['warnings_total']} != {p['warning_count']}"
    )
    assert s["blocked_total"] == len(p["blocked_reasons"]), (
        f"blocked_total mismatch: {s['blocked_total']} != {len(p['blocked_reasons'])}"
    )

    for action in p["actions"]:
        assert "id" in action, f"Action missing id: {action}"
        assert "type" in action, f"Action missing type: {action}"
        assert "target" in action, f"Action missing target: {action}"
        assert "status" in action, f"Action missing status: {action}"
        assert action["status"] == "pending", f"Action not pending: {action}"
        assert "commands" in action, f"Action missing commands: {action}"
        assert isinstance(action["commands"], list), f"commands not list: {action}"
        for cmd in action["commands"]:
            assert "command" in cmd, f"Command entry missing command: {cmd}"
            assert isinstance(cmd["command"], str), f"command not string: {cmd}"

# --- Subtest 11: Human output format ---

with subtest("Human output shows plan summary and command lines"):
    machine.succeed(write_config())
    output = machine.succeed(rust_plan())
    assert "Plan ID:" in output, f"Missing Plan ID:\n{output}"
    assert "Mount:" in output, f"Missing Mount:\n{output}"
    assert "Status:" in output, f"Missing Status:\n{output}"
    assert "applicable" in output, (
        f"Expected 'applicable' in output:\n{output}"
    )
    # disk3 is LUKS-formatted but not in pool → OPEN_LUKS + ADD
    assert "$ cryptsetup open" in output, (
        f"Missing cryptsetup open command line:\n{output}"
    )
    assert "$ btrfs device add" in output, (
        f"Missing btrfs device add command line:\n{output}"
    )

# --- Subtest 12: Bootstrap (unmounted) ---

with subtest("Bootstrap plan for unmounted pool"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup luksClose virtio-disk1 || true")
    machine.succeed("cryptsetup luksClose virtio-disk2 || true")
    machine.succeed("cryptsetup luksClose virtio-disk3 || true")
    machine.succeed("cryptsetup luksClose virtio-disk4 || true")
    machine.succeed(write_config())
    p = rust_plan_json()
    types = [a["type"] for a in p["actions"]]
    assert "OPEN_LUKS" in types, f"Missing OPEN_LUKS:\n{types}"
    assert "ADD_DISK_BTRFS_ADD" in types, f"Missing ADD_DISK_BTRFS_ADD:\n{types}"
    assert "REMOVE_DISK_GRACEFUL" not in types, f"Unexpected REMOVE_DISK_GRACEFUL:\n{types}"
    assert "BALANCE_TO_RAID1" not in types, f"Unexpected BALANCE_TO_RAID1:\n{types}"
    assert p["status"] == "applicable", f"Expected applicable:\n{p}"

# --- Subtest 13: Bootstrap commands use mkfs.btrfs with may_run ---

with subtest("Bootstrap first ADD_DISK uses mkfs.btrfs with may_run certainty"):
    # Still in unmounted state from subtest 12
    add_action = [a for a in p["actions"] if a["type"] == "ADD_DISK_BTRFS_ADD"][0]
    add_cmds = add_action["commands"]
    assert any("mkfs.btrfs" in c["command"] for c in add_cmds), (
        f"Bootstrap ADD_DISK_BTRFS_ADD missing mkfs.btrfs:\n{add_cmds}"
    )
    mkfs_cmd = [c for c in add_cmds if "mkfs.btrfs" in c["command"]][0]
    assert mkfs_cmd.get("certainty") == "may_run", (
        f"Bootstrap mkfs.btrfs should be may_run:\n{mkfs_cmd}"
    )

# --- Subtest 14: Verify actions have empty commands ---

with subtest("Verify actions have empty commands list"):
    # Use a plan that has verify actions — rebuild pool first
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk1 virtio-disk1"
    )
    machine.succeed(
        f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- "
        "/dev/disk/by-id/virtio-disk2 virtio-disk2"
    )
    machine.succeed("btrfs device scan")
    machine.succeed("mount /dev/mapper/virtio-disk1 /mnt/storage")
    machine.succeed(write_config())
    p = rust_plan_json()
    verify_actions = [a for a in p["actions"] if a["type"].startswith("VERIFY_")]
    for va in verify_actions:
        assert va["commands"] == [], (
            f"Verify action should have empty commands: {va}"
        )

machine.shutdown()
