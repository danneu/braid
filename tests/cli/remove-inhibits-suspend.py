# Test: braid remove holds the sleep inhibitor across the 2→1 mutation
# window (RAID1→single pre-balance + btrfs device remove + close)
#
# Intent:
# - What behavior this test verifies.
#   - While `braid remove` is in flight on a 2→1 removal, an inhibitor
#     lock with what=sleep, who=braid, mode=block is registered with
#     logind, with a why string mentioning the disk removal.
#   - The inhibitor remains held while the pre-remove RAID1→single
#     balance is running.
#   - After the remove finishes, the inhibitor lock is released.
#   - The systemd-inhibit + sh + sleep child process group is reaped on
#     teardown, so no orphan PIDs remain.
#   - The pool and payload survive the operation.
#
# Topology choice — 2 disks, removing one:
#   In a 2-disk RAID1 pool, removing one disk leaves only 1 device,
#   which makes RemovePlan::execute in cli/src/remove.rs run
#   pool_balance_single() *before* btrfs device remove. That pre-balance
#   is the long, reliably observable phase. The 3→2 path was tried
#   first and removed: in that topology pool_balance_single is skipped
#   (it only runs when `remaining == 1`), leaving only the kernel
#   `btrfs device remove` step, which on the test runner's fast virtual
#   disks completes well under a second — too fast to observe via
#   external polling. The 2→1 path is the same cmd_remove mutation
#   window with the same inhibitor seam, but with a long enough phase
#   to make the VM test stable.
#
# Why it exists:
# - cmd_remove's mutation window is one of the four storage operations
#   covered by the inhibit-sleep decision (see
#   docs/decisions/019-inhibit-sleep.md). The wiring is identical in shape
#   to cmd_replace's, but a copy-paste regression in cmd_remove would
#   not be caught by replace's existing end-to-end test. The unit tests
#   in cli/src/remove.rs already assert acquire_count == 1; this VM
#   test's job is the *kernel-level handshake*: logind actually receives
#   the inhibitor in production, the why string makes it through, and
#   the systemd-inhibit + sh + sleep process group is torn down on drop
#   without leaking orphans. None of those are observable from
#   RecordingInhibitor.
#
# Scenario:
# - Operator has a 2-disk RAID1 pool with a sizable payload, and runs
#   `braid remove disk2 --yes` (knowingly going to a single-disk,
#   no-redundancy pool). autosuspend would normally request a suspend
#   partway through the multi-minute (or multi-hour on real disks)
#   RAID1→single rebalance. The inhibitor must block that suspend.
#
# list_inhibitors() and find_braid_sleep_inhibitor() come from
# inhibitor_helpers.py, which the .nix harness concatenates onto the front
# of this script at Nix-eval time. They are not imported. shlex is
# imported by inhibitor_helpers.py and already in scope here; re-importing
# it would trip the test driver's lint check.

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


def remove_cmd_bg(key):
    # Background `braid remove` so machine.execute returns immediately.
    # The subshell makes & apply to the whole pipeline.
    return f"(braid remove {key} --yes) > /tmp/remove.log 2>&1 &"


# --- Phase 1: Build a 2-disk RAID1 pool and write a payload ---

with subtest("Build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"2-disk pool btrfs fi df:\n{fi_df}")
    assert "RAID1" in fi_df, f"Expected RAID1 profile on 2-disk pool:\n{fi_df}"

with subtest("Write urandom payload"):
    # 400 MiB gives the pre-remove RAID1→single balance enough relocation
    # work to take observably long on the test runner's virtual disks.
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: Sanity-check that no braid inhibitor exists pre-remove ---

with subtest("No braid inhibitor before remove"):
    pre = list_inhibitors()
    print(f"=== inhibitors pre-remove: {pre} ===")
    assert find_braid_sleep_inhibitor(pre) is None, (
        f"unexpected pre-existing braid sleep inhibitor: {pre}"
    )

# --- Phase 3: Start remove and wait for the balance phase to be in flight ---

with subtest("Start remove and wait for balance to be in flight"):
    machine.execute(remove_cmd_bg("disk2"))

    # Poll for the kernel entering the balance state. The 2→1 remove
    # path runs `pool_balance_single` first, which is the long phase
    # we want to observe. `btrfs balance status` is the same query
    # progress monitoring uses (see cli/src/progress.rs).
    saw_running = False
    last_status = ""
    for _ in range(400):
        ret = machine.execute("btrfs balance status /mnt/storage 2>&1")
        last_status = ret[1]
        if "running" in last_status.lower():
            saw_running = True
            break
        time.sleep(0.05)

    print(f"=== last btrfs balance status ===\n{last_status}")
    assert saw_running, (
        "Never observed btrfs balance in 'running' state during 2→1 "
        "remove — test cannot verify the inhibitor is held during the "
        "pre-remove balance phase. Last status:\n" + last_status
    )

# --- Phase 4: Assert the inhibitor is held while the balance is in flight ---

with subtest("braid sleep inhibitor is held during balance"):
    mid = list_inhibitors()
    print(f"=== inhibitors mid-remove: {mid} ===")
    inh = find_braid_sleep_inhibitor(mid)
    assert inh is not None, (
        "no braid sleep inhibitor found while the pre-remove balance "
        f"is running. inhibitors: {mid}"
    )
    # Bonus locks on the exact shape we expect, so a regression in any
    # field (mode flip, who rename, what widening) fails loudly.
    assert inh["what"] == "sleep", f"expected what=sleep, got {inh!r}"
    assert inh["mode"] == "block", f"expected mode=block, got {inh!r}"
    assert "remov" in inh["why"].lower(), (
        f"expected why mentioning remove, got {inh!r}"
    )
    # Capture the inhibitor pid so the next phase can verify the entire
    # process group is torn down on release (not just the systemd-inhibit
    # parent).
    inhibitor_pid = inh["pid"]

# --- Phase 5: Wait for the remove to finish ---

with subtest("Wait for remove to finish"):
    # pending-op.json clearance is braid's own "operation done" signal
    # (it's the last step of cmd_remove, after journal::clear). More
    # direct than polling /sys/fs/btrfs/<fsid>/exclusive_operation, which
    # can race against braid's post-op cleanup window.
    machine.wait_until_succeeds(
        "test ! -f /var/lib/braid/pending-op.json",
        timeout=600,
    )
    print("=== remove finished ===")

# --- Phase 6: Assert the inhibitor is released after the remove ---
#
# logind releases the inhibitor when the systemd-inhibit child exits, which
# happens when the SleepInhibitor RAII guard in cmd_remove is dropped. Use
# a polling loop to allow a brief settle window — the release is typically
# immediate but should not be timing-fragile.

with subtest("braid sleep inhibitor is released after remove"):
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
            f"braid sleep inhibitor still held 30s after remove finished: {post}"
        )

    post = list_inhibitors()
    print(f"=== inhibitors post-remove: {post} ===")

with subtest("inhibitor process group is torn down (no leaked sh/sleep)"):
    # Regression test: SleepInhibitor::Drop must kill the entire process
    # group, not just the systemd-inhibit parent. See the matching subtest
    # in replace-inhibits-suspend.py for full rationale.
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

with subtest("Pool integrity after remove"):
    machine.succeed("mountpoint -q /mnt/storage")
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed across remove: pre={payload_sha} post={post_sha}"
    )
    fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print("=== final btrfs filesystem show ===")
    print(fs_show)
    assert "MISSING" not in fs_show, (
        f"phantom MISSING device after a clean remove:\n{fs_show}"
    )
    assert "Total devices 1" in fs_show, (
        f"expected 1 device after 2→1 remove, got:\n{fs_show}"
    )
    assert "braid-disk2" not in fs_show, (
        f"removed disk2 still present:\n{fs_show}"
    )
    # After 2→1, the pool must be on the single profile.
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    assert "single" in fi_df.lower(), (
        f"Expected single profile after 2→1 remove:\n{fi_df}"
    )
    assert "raid1" not in fi_df.lower(), (
        f"RAID1 profile should not remain after 2→1 remove:\n{fi_df}"
    )

machine.shutdown()
