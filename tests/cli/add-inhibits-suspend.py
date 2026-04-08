# Test: braid add holds a logind sleep inhibitor across the mutation
# window (LUKS format/open + btrfs device add + pool_balance_raid1)
#
# Intent:
# - What behavior this test verifies.
#   - While `braid add` is in flight, an inhibitor lock with what=sleep,
#     who=braid, mode=block is registered with logind for the entire
#     mutation window — including the long-running `pool_balance_raid1`
#     that converts the pre-existing single-profile data to RAID1.
#   - After add finishes, the inhibitor lock is released.
#   - The systemd-inhibit + sh + sleep child process group is reaped on
#     teardown, so no orphan PIDs remain.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Suspending mid-balance interrupts the conversion of single-profile
#     chunks to RAID1, leaving new data unprotected by redundancy.
#   - braid enables autosuspend by default, so this is reachable in
#     normal operation.
#   - This test fails before the cmd_add inhibitor wiring lands and
#     passes after.
#
# Scenario:
# - Real-world situation this models.
#   - Operator starts with a 1-disk braid pool, writes data to it
#     (single-profile chunks), then adds a second disk to gain RAID1
#     redundancy. The post-add `pool_balance_raid1` is the long-running
#     phase. autosuspend would normally request a suspend partway
#     through. The inhibitor must block that suspend.
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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def add_cmd_bg(key):
    # Background `braid add` so machine.execute returns immediately.
    return (
        f"(printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes) "
        f"> /tmp/add.log 2>&1 &"
    )


# --- Phase 1: Bootstrap a 1-disk pool ---

with subtest("Bootstrap 1-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Pool starts with single-profile data"):
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    print(f"1-disk pool btrfs fi df:\n{fi_df}")
    # 1-disk pool uses single profile by definition.
    assert "single" in fi_df.lower(), (
        f"Expected 'single' profile on 1-disk pool:\n{fi_df}"
    )

# --- Phase 2: Write a single-profile payload ---
#
# This is what gives `pool_balance_raid1` real conversion work. Without it,
# the balance has nothing to do and the inhibitor window collapses.

with subtest("Write urandom payload (single-profile chunks)"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 3: Sanity-check that no braid inhibitor exists pre-add ---

with subtest("No braid inhibitor before add"):
    pre = list_inhibitors()
    print(f"=== inhibitors pre-add: {pre} ===")
    assert find_braid_sleep_inhibitor(pre) is None, (
        f"unexpected pre-existing braid sleep inhibitor: {pre}"
    )

# --- Phase 4: Start `braid add disk2` asynchronously ---

with subtest("Start add and wait for inhibitor to appear"):
    machine.execute(add_cmd_bg("disk2"))

    # Poll for the inhibitor's appearance. LUKS format runs first, then
    # btrfs device add (fast), then the long-running pool_balance_raid1.
    # The inhibitor must be present from the moment the journal is written.
    inh = None
    for _ in range(800):
        inh = find_braid_sleep_inhibitor(list_inhibitors())
        if inh is not None:
            break
        time.sleep(0.05)

    assert inh is not None, (
        "no braid sleep inhibitor observed during add — the inhibitor "
        "seam in cmd_add did not fire, or the entire operation completed "
        "before the polling loop saw it."
    )
    assert inh["what"] == "sleep", f"expected what=sleep, got {inh!r}"
    assert inh["mode"] == "block", f"expected mode=block, got {inh!r}"
    assert "add" in inh["why"].lower(), (
        f"expected why mentioning add, got {inh!r}"
    )
    inhibitor_pid = inh["pid"]

# --- Phase 5: Confirm the inhibitor is held continuously through the
#              balance phase ---
#
# Wait for the kernel to enter the balance state, then re-check that the
# inhibitor is still present. This proves the guard scope covers the
# balance, not just the LUKS format / device add.

with subtest("Inhibitor is still held during the balance phase"):
    saw_balance = False
    for _ in range(800):
        ret = machine.execute(
            "cat /sys/fs/btrfs/*/exclusive_operation 2>&1"
        )
        excl = ret[1].strip().lower()
        if "balance" in excl:
            saw_balance = True
            break
        if "none" in excl:
            print("note: balance phase completed before observation")
            break
        time.sleep(0.05)

    if saw_balance:
        inh_during = find_braid_sleep_inhibitor(list_inhibitors())
        assert inh_during is not None, (
            "braid inhibitor was released before pool_balance_raid1 "
            "completed — the guard scope must cover the balance, not just "
            "the LUKS init and device add."
        )

# --- Phase 6: Wait for add to finish ---

with subtest("Wait for add to finish"):
    # pending-op.json clearance is braid's own "operation done" signal
    # (it's the last step of cmd_add, after journal::clear). More direct
    # than polling /sys/fs/btrfs/<fsid>/exclusive_operation, which can
    # race against braid's post-op cleanup window.
    machine.wait_until_succeeds(
        "test ! -f /var/lib/braid/pending-op.json",
        timeout=600,
    )

# --- Phase 7: Assert the inhibitor is released after add ---

with subtest("braid sleep inhibitor is released after add"):
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
            f"braid sleep inhibitor still held 30s after add finished: {post}"
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

# --- Phase 8: Pool integrity ---

with subtest("Pool integrity after add"):
    machine.succeed("mountpoint -q /mnt/storage")
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed across add: pre={payload_sha} post={post_sha}"
    )
    fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print("=== final btrfs filesystem show ===")
    print(fs_show)
    assert "Total devices 2" in fs_show, (
        f"expected 2 devices after add, got:\n{fs_show}"
    )
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fs_show, (
            f"{name} missing from pool:\n{fs_show}"
        )
    fi_df = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in fi_df, (
        f"Expected RAID1 profile after add+balance:\n{fi_df}"
    )

machine.shutdown()
