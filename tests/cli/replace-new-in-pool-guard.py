# Test: replace --new disk already in pool is caught by braid guard
#
# Intent:
#   `braid replace --old disk1 --new disk2` is rejected with an
#   "already a member" error when disk2 is already a pool member.
#   The rejection must come from braid's own guard, not from btrfs.
#
# Why it exists:
#   The live `btrfs replace start` path has no natural duplicate-device
#   guard. Without check_new_not_in_pool, the command would reach btrfs
#   and either corrupt the pool or produce a confusing btrfs-level error.
#   This test fails when the guard is commented out.
#
# Scenario:
#   Operator typo — specifies an existing pool member as --new.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new} --passphrase-stdin --yes"
    )


with subtest("Setup: build 2-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

with subtest("Replace with existing member rejected by braid guard"):
    (status, output) = machine.execute(replace_cmd("disk1", "disk2") + " 2>&1")
    print(f"Guard output (exit {status}):\n{output}")
    assert status != 0, f"Expected non-zero exit, got 0: {output}"
    assert "already a member" in output, (
        f"Expected braid guard message 'already a member' in output, got:\n{output}"
    )

machine.shutdown()
