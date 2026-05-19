# Test: pool-lock-precedes-state-read
#
# Intent:
#   Every Rust-locked dispatch arm must acquire /run/braid-pool.lock before
#   reading config, membership, pending journals, probes, or passphrases.
#
# Why it exists:
#   The lock is the serialization boundary. If a command reads state before
#   acquiring it, broken local inputs can mask active-operation contention and
#   concurrent writers can observe inconsistent pre-lock state.
#
# Scenario:
#   A separate process holds the pool lock while commands are invoked with
#   deliberately broken pre-lock inputs. The only visible result should be the
#   lock policy for that command.

import time

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)


def with_holder(command, timeout=5, hold_secs=4):
    machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup flock -x /run/braid-pool.lock "
        f"sh -c 'touch /tmp/holder.ready; sleep {hold_secs}' "
        ">/dev/null 2>&1 &"
    )
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    try:
        return machine.execute(f"timeout {timeout} sh -c {command!r} 2>&1")
    finally:
        machine.wait_until_succeeds(
            "flock -n /run/braid-pool.lock true", timeout=hold_secs + 5
        )


def assert_contention(name, command):
    rc, out = with_holder(command)
    assert rc != 0, f"{name}: expected failure; out={out}"
    assert rc != 124, f"{name}: hung before contention; out={out}"
    assert "another braid operation is already in progress" in out, (
        f"{name}: expected contention message; out={out}"
    )
    forbidden = [
        "No such file or directory",
        "failed to read config file",
        "Configuration file not found",
        "Usage:",
    ]
    for needle in forbidden:
        assert needle not in out, f"{name}: read state before lock ({needle}); out={out}"


with subtest("fail-fast mutators acquire before broken config"):
    cases = {
        "unlock": "printf x | braid --config /nonexistent/braid.json unlock --passphrase-stdin",
        "add": "printf x | braid --config /nonexistent/braid.json add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes",
        "recover": "printf x | braid --config /nonexistent/braid.json recover --passphrase-stdin",
        "remove": "braid --config /nonexistent/braid.json remove disk1 --yes",
        "remove-missing": "braid --config /nonexistent/braid.json remove-missing --missing-id 1 --yes",
        "replace": "printf x | braid --config /nonexistent/braid.json replace --old disk1 --new disk2=/dev/disk/by-id/virtio-disk2 --passphrase-stdin --yes",
        "enroll": "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin",
        "lock": "braid --config /nonexistent/braid.json lock",
    }
    for name, command in cases.items():
        assert_contention(name, command)

with subtest("discover --write acquires before pending-op and probe reads"):
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("printf '{\"op\":\"placeholder\"}' > /var/lib/braid/pending-op.json")
    rc, out = with_holder("braid discover --write --expect-count 1")
    machine.succeed("rm -f /var/lib/braid/pending-op.json")
    assert rc != 0, "discover --write should fail under contention; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert "pending-op.json exists" not in out, (
        "discover read pending-op before acquiring lock; out=" + out
    )
    assert "no braid-labeled LUKS devices found" not in out, (
        "discover probed devices before acquiring lock; out=" + out
    )

with subtest("ack waits then reports contention before broken config"):
    start = time.monotonic()
    rc, out = with_holder(
        "braid --config /nonexistent/braid.json ack", timeout=15, hold_secs=12
    )
    elapsed = time.monotonic() - start
    assert rc != 0, "ack should fail after bounded wait; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert "failed to read config file" not in out, (
        "ack read config before acquiring lock; out=" + out
    )
    assert 9 <= elapsed <= 14, f"ack wait outside expected window: {elapsed:.2f}s"

with subtest("monitor exits silently before broken config"):
    rc, out = with_holder("braid --config /nonexistent/braid.json monitor")
    assert rc == 0, "monitor should silently skip held-lock cycle; out=" + out
    assert out.strip() == "", "monitor contention should be silent; out=" + out
