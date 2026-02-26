# Test: braid-wrong-passphrase-resume
#
# Intent:
#   Verify that a wrong passphrase during a `braid add` resume attempt fails
#   with a clear, user-readable error and leaves the checkpoint file intact so
#   the user can fix their environment and retry.
#
# Why it exists:
#   Without this guard a regression could silently clear or corrupt the
#   checkpoint on a wrong-passphrase attempt, making recovery impossible.
#
# Scenario:
#   User adds disk1 (single-disk pool), then begins adding disk2 but the
#   process is interrupted after the checkpoint is saved. They then retry
#   with the wrong passphrase. The checkpoint must survive the failed attempt
#   so that a final retry with the correct passphrase can complete the add.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add(key, pw=None, env=""):
    pw = pw or passphrase
    pw_q = shlex.quote(pw)
    env_parts = f"BRAID_LUKS_OPTS='{luks_opts}'"
    if env:
        env_parts += f" {env}"
    return f"printf '%s\\n' {pw_q} | {env_parts} braid add {key} --passphrase-stdin --yes"


with subtest("Setup: create initial single-disk pool"):
    machine.succeed(add("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Interrupt add disk2 after checkpoint is saved"):
    machine.fail(add("disk2", env="BRAID_TEST_FAIL_AFTER_CHECKPOINT=1") + " 2>&1")
    machine.succeed("test -f /var/lib/braid/op-state.json")

with subtest("Wrong passphrase on resume fails with clear error"):
    output = machine.fail(add("disk2", pw="wrongpassphrase") + " 2>&1")
    assert "passphrase" in output.lower(), f"expected passphrase error:\n{output}"

with subtest("Checkpoint intact after wrong-passphrase resume attempt"):
    machine.succeed("test -f /var/lib/braid/op-state.json")

with subtest("Resume with correct passphrase succeeds"):
    machine.succeed(add("disk2"))
    machine.fail("test -f /var/lib/braid/op-state.json")
    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df, f"expected RAID1 after resume:\n{df}"

machine.shutdown()
