# Test: braid remove-missing wires the sleep inhibitor across its
# mutation window
#
# Intent:
# - What behavior this test verifies.
#   - While `braid remove-missing` is in flight, an inhibitor lock with
#     what=sleep, who=braid, mode=block is registered with logind, with
#     a who string mentioning the missing-device removal.
#   - After remove-missing finishes, the inhibitor lock is released.
#   - The systemd-inhibit + sh + sleep child process group is reaped on
#     teardown, so no orphan PIDs remain.
#
# What this test does NOT verify:
# - This test does not exercise a meaningful long-running soft-balance
#   workload. In a 3-disk RAID1 pool with only 1 missing device, btrfs
#   can still write RAID1 chunks (2 surviving disks satisfy the mirror
#   count — see tests/repro/degraded-writes-3disk.py), so degraded
#   writes do not produce single-profile chunks the way the proven
#   2-disk tests/repro/degraded-soft-balance.py scenario does. The
#   soft balance fired by maybe_restore_raid1 is therefore a near-no-op
#   in this topology, and the entire mutation window is fast (~hundreds
#   of ms in this fixture. Real `btrfs device remove <devid>` can still
#   relocate chunks and take minutes when the missing device had data
#   allocated; this test does not depend on that duration.
#
#   We still need a 3-disk pool because maybe_restore_raid1's "≥2
#   surviving devices" gate would not fire on a 2-disk pool (which
#   would leave 1 device after the missing one is cleared). Constructing
#   a topology that *both* satisfies that gate *and* produces
#   single-profile chunks would require a multi-step rebuild that
#   substantially complicates the test for marginal coverage benefit.
#
#   The unit test in cli/src/remove_missing.rs already asserts
#   acquire_count == 1 across this exact code path. This VM test's job
#   is the *kernel-level handshake*: logind actually receives and
#   registers the inhibitor in production, the why string makes it
#   through, and the systemd-inhibit + sh + sleep process group is
#   torn down on drop without leaking orphans. None of those are
#   observable from RecordingInhibitor.
#
# Why it exists:
# - What risk/regression this protects against.
#   - cmd_remove_missing's mutation window is one of the four storage
#     operations covered by the inhibit-sleep decision (see
#     docs/design/decisions/019-inhibit-sleep.md). The wiring is identical in
#     shape to cmd_replace's, but a copy-paste regression in
#     cmd_remove_missing would not be caught by replace's existing
#     end-to-end test.
#
# Scenario:
# - 3-disk NAS, one drive dies. Operator mounts the pool degraded and
#   runs `braid remove-missing` to clear the missing entry. The full
#   019-inhibit-sleep.md justification (autosuspend racing the mutation
#   window) is what motivates the wiring; this test verifies the
#   wiring is in place.
#
# Catching the brief inhibitor window:
#   The mutation window is short, so we tight-poll list_inhibitors()
#   at 10 ms intervals for up to 5 seconds.
#
# Missing-disk setup reuses the canonical pattern from
# tests/cli/braid-remove-disk.py: umount → cryptsetup close → mount -o
# degraded.
#
# list_inhibitors() and find_braid_sleep_inhibitor() come from
# inhibitor_helpers.py, which the .nix harness concatenates onto the front
# of this script at Nix-eval time. They are not imported. shlex is
# imported by inhibitor_helpers.py and already in scope here; re-importing
# it would trip the test driver's lint check.

import json
import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def get_missing_devid():
    """Get the devid of the missing device from braid status --json.
    Lifted from tests/cli/braid-remove-disk.py."""
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) > 0, "No missing devids in braid status:\n" + raw
    return str(devids[0])


def remove_missing_cmd_bg(devid):
    return (
        f"(braid remove-missing --missing-id {devid} --yes) "
        f"> /tmp/remove-missing.log 2>&1 &"
    )


# --- Phase 1: Build a 3-disk pool and write a small payload ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write a small payload to anchor pool integrity check"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=20 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: Simulate disk3 death and mount degraded ---
#
# Canonical pattern from tests/cli/braid-remove-disk.py: umount →
# cryptsetup close → mount -o degraded. Simulated rather than physical
# hot-unplug.

with subtest("Simulate disk3 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"Pool after disk3 death:\n{fi_show}")
    assert "missing" in fi_show.lower(), (
        f"Expected 'missing' in btrfs filesystem show:\n{fi_show}"
    )

# --- Phase 3: Sanity-check that no braid inhibitor exists pre-remove ---

with subtest("No braid inhibitor before remove-missing"):
    pre = list_inhibitors()
    print(f"=== inhibitors pre-remove-missing: {pre} ===")
    assert find_braid_sleep_inhibitor(pre) is None, (
        f"unexpected pre-existing braid sleep inhibitor: {pre}"
    )

# --- Phase 4: Start remove-missing and tightly poll for inhibitor ---

with subtest("Resolve missing devid"):
    missing_devid = get_missing_devid()
    print(f"missing devid: {missing_devid}")

with subtest("Start remove-missing and catch the brief inhibitor window"):
    machine.execute(remove_missing_cmd_bg(missing_devid))

    # Tight polling: 10 ms intervals, up to 5 seconds. The mutation
    # window in this fixture is short (device remove + near-noop soft
    # balance), so we need fine granularity to catch the inhibitor before
    # braid drops it.
    inh = None
    for _ in range(500):
        inh = find_braid_sleep_inhibitor(list_inhibitors())
        if inh is not None:
            break
        time.sleep(0.01)

    assert inh is not None, (
        "no braid sleep inhibitor observed during remove-missing — the "
        "inhibitor seam in cmd_remove_missing did not fire, or the entire "
        "operation completed within 5 seconds before any of the 500 polls "
        "caught it."
    )
    assert inh["what"] == "sleep", f"expected what=sleep, got {inh!r}"
    assert inh["mode"] == "block", f"expected mode=block, got {inh!r}"
    assert "missing" in inh["why"].lower(), (
        f"expected why mentioning missing device, got {inh!r}"
    )
    inhibitor_pid = inh["pid"]

# --- Phase 5: Wait for remove-missing to finish ---

with subtest("Wait for remove-missing to finish"):
    # pending-op.json clearance is braid's own "operation done" signal
    # (it's the last step of cmd_remove_missing, after journal::clear).
    # More direct than polling /sys/fs/btrfs/<fsid>/exclusive_operation,
    # which can race against braid's post-op cleanup window.
    machine.wait_until_succeeds(
        "test ! -f /var/lib/braid/pending-op.json",
        timeout=300,
    )

# --- Phase 6: Assert the inhibitor is released after remove-missing ---

with subtest("braid sleep inhibitor is released after remove-missing"):
    def no_braid_inhibitor():
        return find_braid_sleep_inhibitor(list_inhibitors()) is None

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if no_braid_inhibitor():
            break
        time.sleep(0.5)
    else:
        post = list_inhibitors()
        raise AssertionError(
            f"braid sleep inhibitor still held 30s after remove-missing finished: {post}"
        )

with subtest("inhibitor process group is torn down (no leaked sh/sleep)"):
    def pgroup_empty():
        ret = machine.execute(f"pgrep -g {inhibitor_pid}")
        return ret[0] != 0

    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if pgroup_empty():
            break
        time.sleep(0.2)
    else:
        leaked = machine.execute(f"pgrep -agf -g {inhibitor_pid} 2>&1")[1]
        raise AssertionError(
            f"process group {inhibitor_pid} still has live members 10s "
            f"after the inhibitor was released — SleepInhibitor::Drop "
            f"leaked one or more children:\n{leaked}"
        )

# --- Phase 7: Pool integrity ---

with subtest("Pool integrity after remove-missing"):
    machine.succeed("mountpoint -q /mnt/storage")
    post_payload = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_payload == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_payload}"
    )
    fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print("=== final btrfs filesystem show ===")
    print(fs_show)
    assert "missing" not in fs_show.lower(), (
        f"missing device still present after remove-missing:\n{fs_show}"
    )

machine.shutdown()
