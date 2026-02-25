# Test: checkpoint op-state strict resumability
#
# Intent:
# - What behavior this test (tries to) verify.
#   - Checkpoint/resume safety contracts hold end-to-end for intent CLI, including
#     deterministic interruption handling and explicit fail-closed rejects.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Protects against silent resume misbehavior, weak error surfaces, and
#     non-deterministic interruption handling in long-running operations.
#
# Scenario:
# - Real-world situation this models (user/system story). Especially the
#   specific scenario that inspired this test (like a real world bug).
#   - User runs `braid add/remove`, process is interrupted, then retried; system
#     must either resume safely or reject with explicit checkpoint error codes.
#
# What: Validates strict checkpoint behavior for intent commands: deterministic
# fail-after-checkpoint resume, strict config-drift rejection with stable error
# code format, and bounded pause timeout behavior.
#
# Why: Checkpoints are a safety boundary. Invalid resumes must fail closed and
# interruption hooks must be deterministic and CI-safe.
#
# Dependencies: `braid add` lifecycle and RAID1 balance behavior.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
config_path = "/tmp/braid-config.json"


def write_config(mount_point="/mnt/storage"):
    config = {
        "disks": {
            "disk1": {"by_id": "/dev/disk/by-id/virtio-disk1"},
            "disk2": {"by_id": "/dev/disk/by-id/virtio-disk2"},
            "disk3": {"by_id": "/dev/disk/by-id/virtio-disk3"},
            "disk4": {"by_id": "/dev/disk/by-id/virtio-disk4"},
        },
        "mount_point": mount_point,
    }
    escaped = json.dumps(config).replace("'", "'\\''")
    machine.succeed(f"echo '{escaped}' > {config_path}")


def add(name, env=""):
    prefix = (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
    )
    if env:
        prefix += f"{env} "
    return f"{prefix}braid --config {config_path} add {name} --yes"


def remove(name, env=""):
    prefix = ""
    if env:
        prefix = f"{env} "
    return f"{prefix}braid --config {config_path} remove {name} --yes"


def fail_with_stderr(cmd):
    return machine.fail(f"{cmd} 2>&1")


with subtest("Setup: create initial single-disk pool"):
    write_config()
    machine.succeed(add("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Interrupted add leaves checkpoint and resume succeeds"):
    fail_with_stderr(add("disk2", env="BRAID_TEST_FAIL_AFTER_CHECKPOINT=1"))
    machine.succeed("test -f /var/lib/braid/op-state.json")
    phase = machine.succeed("jq -r '.phase' /var/lib/braid/op-state.json").strip()
    assert phase == "add-balance-raid1", f"unexpected phase: {phase}"

    machine.succeed(add("disk2"))
    machine.fail("test -f /var/lib/braid/op-state.json")
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"expected RAID1 after resume:\n{df_output}"

with subtest("Interrupted remove leaves checkpoint and resume succeeds"):
    fail_with_stderr(remove("disk2", env="BRAID_TEST_FAIL_AFTER_CHECKPOINT=1"))
    machine.succeed("test -f /var/lib/braid/op-state.json")
    phase = machine.succeed("jq -r '.phase' /var/lib/braid/op-state.json").strip()
    assert phase == "remove-start", f"unexpected phase: {phase}"

    machine.succeed(remove("disk2"))
    machine.fail("test -f /var/lib/braid/op-state.json")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk2" not in fi_show, f"disk2 should be removed after resume:\n{fi_show}"

with subtest("Config drift rejects checkpoint with stable error code format"):
    fail_with_stderr(add("disk3", env="BRAID_TEST_FAIL_AFTER_CHECKPOINT=1"))
    machine.succeed("test -f /var/lib/braid/op-state.json")

    write_config(mount_point="/mnt/storage-drifted")
    output = fail_with_stderr(add("disk3"))
    assert "error[CHECKPOINT_CONFIG_DRIFT]:" in output, (
        f"expected strict config drift error code:\n{output}"
    )

    machine.succeed("test -f /var/lib/braid/op-state.json")
    phase = machine.succeed("jq -r '.phase' /var/lib/braid/op-state.json").strip()
    assert phase == "add-balance-raid1", f"checkpoint phase changed unexpectedly: {phase}"

with subtest("Pause hook times out with deterministic error code"):
    write_config()
    machine.succeed("rm -f /var/lib/braid/op-state.json")
    output = fail_with_stderr(
        add(
            "disk4",
            env=(
                "BRAID_TEST_PAUSE_AT_PHASE=add-balance-raid1 "
                "BRAID_TEST_PAUSE_TIMEOUT_SECS=1 "
                "BRAID_TEST_PAUSE_FILE=/tmp/never-created"
            ),
        )
    )
    assert "error[CHECKPOINT_PAUSE_TIMEOUT]:" in output, (
        f"expected pause timeout error code:\n{output}"
    )

machine.shutdown()
