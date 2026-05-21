# Test: pool-lock-dry-run-bypass
#
# Intent:
#   Every --dry-run mutator bypasses /run/braid-pool.lock. Dispatch must not
#   acquire under dry-run, so preview is safe to run while another operation
#   holds the lock.
#
# Why it exists:
#   `lock_policy` centralizes the dispatch-time policy decision and returns
#   None for each mutator under --dry-run. The classification is unit-tested,
#   but runtime bypass behavior was not exercised end-to-end for the dry-run
#   mutators. A regression that re-introduces per-arm lock acquisition, or
#   special-cases dry-run inside acquire_per_policy, would break the "safe to
#   preview at any time" UX contract.
#
# Scenario:
#   An admin starts a long-running pool mutation, then runs
#   `braid <mutator> --dry-run` from another shell. The preview should not
#   block on the held flock and should not print the contention message.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)


def with_holder(command, timeout=2, hold_secs=8):
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        f"sh -c 'touch /tmp/holder.ready; sleep {hold_secs}' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, "no flock in /proc/locks: " + locks
    try:
        return machine.execute(f"timeout {timeout} sh -c {command!r} 2>&1")
    finally:
        machine.wait_until_succeeds(
            "flock -n /run/braid-pool.lock true", timeout=hold_secs + 5
        )


def assert_no_contention(name, command):
    rc, out = with_holder(command)
    assert rc != 124, (
        f"{name}: dry-run blocked on the held lock for >2s "
        "(holder held for 8s, so the command was demonstrably waiting "
        f"on acquire, not running); out={out}"
    )
    assert "another braid operation is already in progress" not in out, (
        f"{name}: dry-run acquired the pool lock; out={out}"
    )
    assert "Usage:" not in out, (
        f"{name}: clap rejected the invocation before dispatch -- "
        f"fix the command shape; out={out}"
    )
    assert "must be run as root" not in out, (
        f"{name}: root check fired before dispatch; out={out}"
    )


with subtest("dry-run mutators bypass /run/braid-pool.lock"):
    cases = {
        "add": "printf x | braid --config /nonexistent/braid.json add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes --dry-run",
        "remove": "braid --config /nonexistent/braid.json remove disk1 --yes --dry-run",
        "remove-missing": "braid --config /nonexistent/braid.json remove-missing --missing-id 1 --yes --dry-run",
        "replace": "printf x | braid --config /nonexistent/braid.json replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes --dry-run",
        "unlock": "printf x | braid --config /nonexistent/braid.json unlock --passphrase-stdin --dry-run",
        "enroll": "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin --dry-run",
        "recover": "printf x | braid --config /nonexistent/braid.json recover --passphrase-stdin --dry-run",
        "lock": "braid --config /nonexistent/braid.json lock --dry-run",
    }
    for name, command in cases.items():
        assert_no_contention(name, command)
