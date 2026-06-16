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
        "lock": "braid --config /nonexistent/braid.json lock",
    }
    for name, command in cases.items():
        assert_contention(name, command)

with subtest("discover --write acquires before pending-op and probe reads"):
    # Intent: under contention, discover --write must exit at the central lock
    #   acquire (cli/src/main.rs#acquire_per_policy, before the dispatch match)
    #   before it reads the pending-op journal or probes /dev/disk/by-id/.
    # Why: ADR 018 / principle 12
    #   (`docs/design/principles.md#12-one-pool-operation-at-a-time`) make lock
    #   acquire the serialization boundary. The two negative sentinels below each
    #   catch a DIFFERENT pre-lock leak, primed differently:
    #     - pending-op: primed by the planted placeholder journal -- a pre-lock
    #       read routes through the canonical recovery-mode guard, which fails
    #       to parse the placeholder and errors "cannot read pending-op.json".
    #     - probe: primed by this host discovering ZERO braid-labeled LUKS2
    #       members (the .nix is diskless; see pool-lock-precedes-state-read.nix).
    #       The baseline below proves that precondition by observation rather than
    #       assuming it, so a discoverable member appearing later fails here loudly
    #       instead of silently neutering the guard.
    # Scenario: external holder holds /run/braid-pool.lock; discover --write runs
    #   with a placeholder pending-op.json planted. Nonblocking flock fails fast
    #   with the contention message before either read.
    machine.succeed("mkdir -p /var/lib/braid")
    # Keep the empty-scan baseline robust against pool.json drift: after Edit 3
    # the ValidUuidKeyed gate runs before the scan, so a stray healthy pool.json
    # would refuse with "is already a healthy UUID-keyed membership" instead of
    # the empty-scan refusal and trip the "did a discoverable member appear?"
    # assertion below. Diskless host -> pool.json is normally absent; this just
    # pins it. Matches the defensive rm -f the FIFO subtest already uses.
    machine.succeed("rm -f /var/lib/braid/pool.json")
    # Single source of truth for the probe sentinel's substring -- asserted
    # PRESENT in the baseline, ABSENT under contention. Tracks the lead clause of
    # cli/src/discover.rs#NoMembersDiscovered (not the remediation tail, which may
    # reword freely). Because the baseline asserts it PRESENT, a stale value here
    # -- renamed in NoMembersDiscovered but not updated -- now fails the baseline
    # loudly instead of silently retiring the negative sentinel.
    refusal = "no braid-labeled LUKS2 devices found"
    # Baseline (no contention): the probe runs, finds zero discoverable
    # braid-labeled LUKS2 members, and discover --write prints the empty-scan
    # refusal, exiting at the is_empty gate before writing anything. This positive
    # half is what makes the negative probe sentinel under contention meaningful.
    # --expect-count 9999 keeps the baseline fail-closed against fixture drift:
    # if a discoverable member ever appears, write_discovered_membership refuses
    # with ExpectCountUnmet (count != 9999) before save_membership -- so no
    # pool.json is written, and base_out carries that error instead of the
    # refusal, tripping the "precondition broken" assertion below rather than
    # silently writing state. The positive-impossible count matters because
    # --expect-count 0 is rejected at parse time before lock acquire and probe.
    base_rc, base_out = machine.execute("braid discover --write --expect-count 9999 2>&1")
    assert base_rc != 0, "baseline should exit nonzero (empty-scan refusal); out=" + base_out
    assert refusal in base_out, (
        "precondition broken: expected the empty-scan refusal without contention "
        "(did a discoverable braid-labeled LUKS2 member appear in the .nix?); "
        "out=" + base_out
    )
    machine.succeed("printf '{\"op\":\"placeholder\"}' > /var/lib/braid/pending-op.json")
    rc, out = with_holder("braid discover --write --expect-count 9999")
    machine.succeed("rm -f /var/lib/braid/pending-op.json")
    assert rc != 0, "discover --write should fail under contention; out=" + out
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
    assert "cannot read pending-op.json" not in out, (
        "discover read pending-op before acquiring lock; out=" + out
    )
    assert refusal not in out, (
        "discover probed devices before acquiring lock; out=" + out
    )

with subtest("discover --write does not read pool.json before acquiring lock"):
    # Intent: pool.json must not be opened before the pool lock is held.
    # Why: principle 12 (`docs/design/principles.md#12-one-pool-operation-at-a-time`) and ADR 018 require lock acquire
    # to precede any pool.json load on locked dispatch arms. A regression that
    # reads pool.json pre-lock would not be caught by the existing pending-op /
    # probe assertions because the classify result is discarded on --write and
    # produces no diagnostic.
    # Scenario: external holder holds /run/braid-pool.lock; pool.json is a
    # blocking FIFO with no writer. Under the invariant, the nonblocking flock
    # acquire fails fast and the command exits with the contention message before
    # any pool.json read. A regression that moves classify_pool_json, or any
    # other pool.json read, above the lock acquire would block on FIFO open and
    # time out.
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("rm -f /var/lib/braid/pool.json")
    machine.succeed("mkfifo /var/lib/braid/pool.json")
    try:
        rc, out = with_holder("braid discover --write --expect-count 1")
        assert rc != 0, "discover --write should fail under contention; out=" + out
        assert rc != 124, (
            "discover --write read pool.json before acquiring lock (timed out); "
            "out=" + out
        )
        assert "another braid operation is already in progress" in out, (
            "expected contention message; out=" + out
        )
    finally:
        machine.succeed("rm -f /var/lib/braid/pool.json")

with subtest("enroll acquires before membership read"):
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("printf 'not valid json' > /var/lib/braid/pool.json")
    rc, out = with_holder(
        "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin"
    )
    machine.succeed("rm -f /var/lib/braid/pool.json")
    assert rc != 0, "enroll should fail under contention; out=" + out
    assert rc != 124, "enroll hung past contention; out=" + out
    assert "pool membership file corrupt at" not in out, (
        "enroll read membership before acquiring lock; out=" + out
    )
    assert "failed to read pool membership file at" not in out, (
        "enroll read membership before acquiring lock; out=" + out
    )
    assert "failed to read config file" not in out, (
        "enroll read config before acquiring lock; out=" + out
    )
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
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
