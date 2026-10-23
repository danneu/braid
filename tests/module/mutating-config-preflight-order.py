# Test: mutating-config-preflight-order
#
# Intent:
#   Every command that recovery mode refuses checks pending-op.json before it
#   reads config or pool.json, and config-load failures in the add, remove,
#   remove-missing and replace arms use the command-style "config error:"
#   prefix.
#
# Why it exists:
#   Config loading moved out of add/remove/remove-missing/replace planners into
#   dispatch. The recovery journal must keep priority over config errors, and
#   add's real-run config error wording intentionally changed to match dry-run
#   and the other mutators. unlock and enroll once loaded config and pool.json
#   in their dispatch arms before the journal check, so a missing or corrupt
#   pool.json hid the journal behind an error that never said `braid recover`
#   (finding 261022-0632-001).
#
# Scenario:
#   An operator invokes mutating commands while pending-op.json exists, or while
#   the configured config file is missing. The first case must tell them to run
#   recover; the second must report a config error consistently. A bootstrap
#   `braid add` is interrupted after its journal write and before pool.json is
#   saved; at the next boot braid-auto-unlock runs `braid unlock`, and the
#   operator runs `braid enroll`. Both must name the interrupted operation, not
#   the missing pool.json, and a corrupt pool.json must not send the operator to
#   `braid discover --write`, which recovery mode refuses.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

missing_config = "/tmp/braid-missing-config.json"
valid_config = "/etc/braid/config.json"
pending_json = r'''{"started_at":"2024-01-01T00:00:00Z","op":{"op":"Add","phase":"PoolMutation","targets":{}},"pre_membership":{"disks":{}},"target_membership":{"disks":{}}}'''

base_cases = [
    ("add", "add disk3=/dev/disk/by-id/virtio-disk3 --yes"),
    ("remove", "remove disk1 --yes"),
    ("remove-missing", "remove-missing --missing-id 1 --yes"),
    (
        "replace",
        "replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --yes",
    ),
]

config_error_cases = [
    ("add", "add disk3=/dev/disk/by-id/virtio-disk3 --yes"),
    ("add --dry-run", "add disk3=/dev/disk/by-id/virtio-disk3 --dry-run --yes"),
    ("remove", "remove disk1 --yes"),
    ("remove --dry-run", "remove disk1 --dry-run --yes"),
    ("remove-missing", "remove-missing --missing-id 1 --yes"),
    ("remove-missing --dry-run", "remove-missing --missing-id 1 --dry-run --yes"),
    (
        "replace",
        "replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --yes",
    ),
    (
        "replace --dry-run",
        "replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --dry-run --yes",
    ),
]


# unlock and enroll load config or pool.json in dispatch, so each case pairs a
# pending journal with one broken state file. stdin carries a passphrase so a
# command that got past the journal check would not wait on a TTY prompt.
recovery_gate_cases = [
    ("unlock", "unlock --passphrase-stdin"),
    ("unlock --dry-run", "unlock --dry-run --passphrase-stdin"),
    ("enroll", "enroll /mnt/usb --passphrase-stdin"),
    ("enroll --dry-run", "enroll /mnt/usb --dry-run --passphrase-stdin"),
]


def command(args, config):
    return "braid --config " + shlex.quote(config) + " " + args + " 2>&1"


def run_case(args, config):
    return machine.execute(command(args, config))


def run_case_with_stdin(args, config):
    return machine.execute("printf x | " + command(args, config))


def clear_pending():
    machine.succeed("rm -f /var/lib/braid/pending-op.json")


def write_pending():
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed(
        "cat > /var/lib/braid/pending-op.json <<'JOURNAL'\n"
        + pending_json
        + "\nJOURNAL"
    )


def seed_locked_pool_json():
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed(
        "cat > /var/lib/braid/pool.json <<'POOL'\n"
        '{"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}'
        "\nPOOL"
    )


def remove_pool_json():
    machine.succeed("rm -f /var/lib/braid/pool.json")


def write_corrupt_pool_json():
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("printf '{not valid json}' > /var/lib/braid/pool.json")


def assert_recovery_gate_wins(state, config):
    for name, args in recovery_gate_cases:
        label = name + " (" + state + ")"
        rc, out = run_case_with_stdin(args, config)
        assert rc == 1, label + ": expected exit 1, got " + str(rc) + "; out=" + out
        assert "interrupted operation detected" in out, (
            label + ": expected pending-op guidance, got: " + out
        )
        assert "failed to read config file" not in out, (
            label + ": config loaded before the journal check: " + out
        )
        assert "failed to read pool membership file" not in out, (
            label + ": pool.json loaded before the journal check: " + out
        )
        assert "discover --write" not in out, (
            label + ": pool.json error preempted pending-op: " + out
        )


with subtest("Missing config reports wrapped config errors"):
    clear_pending()
    machine.fail("test -e " + shlex.quote(missing_config))
    for name, args in config_error_cases:
        rc, out = run_case(args, missing_config)
        assert rc == 1, name + ": expected exit 1, got " + str(rc) + "; out=" + out
        assert "error: config error: failed to read config file " + missing_config in out, (
            name + ": expected wrapped config error, got: " + out
        )
        assert "interrupted operation detected" not in out, (
            name + ": unexpected pending-op message, got: " + out
        )

with subtest("Pending operation wins over missing config"):
    write_pending()
    for name, args in base_cases:
        rc, out = run_case(args, missing_config)
        assert rc == 1, name + ": expected exit 1, got " + str(rc) + "; out=" + out
        assert "interrupted operation detected" in out, (
            name + ": expected pending-op guidance, got: " + out
        )
        assert "config error" not in out, name + ": config loaded before preflight: " + out

with subtest("Pending operation wins with valid config"):
    write_pending()
    for name, args in base_cases:
        rc, out = run_case(args, valid_config)
        assert rc == 1, name + ": expected exit 1, got " + str(rc) + "; out=" + out
        assert "interrupted operation detected" in out, (
            name + ": expected pending-op guidance, got: " + out
        )
        assert "config error" not in out, name + ": unexpected config error: " + out

with subtest("Add pending operation wins over locked-pool refusal"):
    clear_pending()
    seed_locked_pool_json()
    rc, out = run_case(
        "add disk3=/dev/disk/by-id/virtio-disk3 --dry-run --yes", valid_config
    )
    assert rc == 1, "locked-pool sanity should fail; out=" + out
    assert "not unlocked" in out, "sanity must reach locked-pool refusal; out=" + out

    write_pending()
    rc, out = run_case(
        "add disk3=/dev/disk/by-id/virtio-disk3 --dry-run --yes", valid_config
    )
    assert rc == 1, "pending add should fail; out=" + out
    assert "interrupted operation detected" in out, (
        "pending-op guidance must win, got: " + out
    )
    assert "not unlocked" not in out, "locked-pool refusal preempted pending-op: " + out

with subtest("Pending operation wins over a missing config for unlock and enroll"):
    seed_locked_pool_json()
    write_pending()
    assert_recovery_gate_wins("missing config", missing_config)

with subtest("Pending operation wins over a missing pool.json for unlock and enroll"):
    remove_pool_json()
    write_pending()
    assert_recovery_gate_wins("missing pool.json", valid_config)

with subtest("Pending operation wins over a corrupt pool.json for unlock and enroll"):
    write_corrupt_pool_json()
    write_pending()
    assert_recovery_gate_wins("corrupt pool.json", valid_config)

machine.shutdown()
