# Test: braid replace holds a logind sleep inhibitor for the duration
#
# Intent:
# - What behavior this test verifies.
#   - While `braid replace` is in flight, an inhibitor lock with what=sleep,
#     who=braid, mode=block is registered with logind, preventing the host
#     from suspending mid-replace.
#   - After the replace finishes, that inhibitor lock is released.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Suspending mid-replace produces kernel-level topology corruption on
#     every kernel: the kernel resume-on-mount path finishes the data copy
#     but does not perform the post-completion devid swap, leaving a phantom
#     MISSING devid 0 in the pool (Path A in tracking issue #48). On v6.19+
#     kernels it also triggers the new freeze/signal cancellation path
#     (Path B), forcing the user to restart the replace from scratch.
#   - Upstream btrfs explicitly recommends inhibiting suspend during replace
#     — reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50. braid
#     enables autosuspend by default, so this interaction is reachable in
#     normal operation.
#   - This test fails before the SleepInhibitor helper lands and passes
#     after.
#
# Scenario:
# - Real-world situation this models.
#   - Operator starts a `braid replace` to swap a healthy drive while their
#     NAS is configured with default autosuspend. autosuspend would normally
#     decide the system is idle and request a suspend partway through the
#     multi-hour replace. The inhibitor must block that suspend until the
#     replace (and its post-replace soft balance, if applicable) finishes.

# list_inhibitors() and find_braid_sleep_inhibitor() come from
# inhibitor_helpers.py, which the .nix harness concatenates onto the front
# of this script at Nix-eval time. They are not imported — the helpers and
# this file are joined into a single script string before the runner sees
# them. shlex is imported by inhibitor_helpers.py, so it is already in
# scope here; re-importing it would trip the test driver's lint check.

import time

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"
DELAYED_DISKS = {"disk4"}


def disk_path(key):
    if key in DELAYED_DISKS:
        return f"/dev/disk/by-id/braid-test-{key}-delay"
    return f"/dev/disk/by-id/virtio-{key}"


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}={disk_path(key)} --passphrase-stdin --yes"
    )


def replace_cmd_bg(old, new):
    # Background the entire (passphrase | braid replace) pipeline so
    # machine.execute returns immediately. The subshell makes & apply to
    # the whole pipeline rather than just the tail braid invocation.
    return (
        f"(printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}={disk_path(new)} "
        f"--passphrase-stdin --yes) > /tmp/replace.log 2>&1 &"
    )


# --- Phase 1: Build a 3-disk pool and write a payload ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=16 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: Sanity-check that no braid inhibitor exists pre-replace ---

with subtest("No braid inhibitor before replace"):
    pre = list_inhibitors()
    print(f"=== inhibitors pre-replace: {pre} ===")
    assert find_braid_sleep_inhibitor(pre) is None, (
        f"unexpected pre-existing braid sleep inhibitor: {pre}"
    )

# --- Phase 3: Start replace asynchronously and wait for in-flight progress ---

with subtest("Start replace and wait for in-flight progress"):
    dm_delay_create(machine, "disk4")
    dm_delay_activate(machine, "disk4", write_delay_ms=200)
    machine.execute(replace_cmd_bg("disk2", "disk4"))

    saw_running = False
    last_status = ""
    for _ in range(400):
        ret = machine.execute("btrfs replace status -1 /mnt/storage 2>&1")
        last_status = ret[1]
        if "Started on" in last_status or "% done" in last_status:
            saw_running = True
            break
        time.sleep(0.05)

    print("=== last btrfs replace status ===")
    print(last_status)
    assert saw_running, (
        "Never observed btrfs replace in-flight — test cannot verify the "
        "inhibitor is held during the replace. Last status:\n" + last_status
    )

# --- Phase 3a: braid idle must return promptly + detect the replace via sysfs ---
#
# Intent: End-to-end check that `braid idle` detects an in-flight replace
# via /sys/fs/btrfs/<fsid>/exclusive_operation and returns busy. The 5 s
# timeout is a promptness check on the sysfs read path -- a regression
# that re-introduces a blocking subprocess probe (e.g. dropping `-1` from
# BtrfsReplaceStatus and calling it from idle again) would surface here.
#
# Why it exists: idle.rs used to drive BtrfsReplaceStatus directly, and
# cli/src/cmd.rs was missing the `-1` flag, so `braid idle` blocked
# indefinitely when a replace was in flight. The fix moved idle.rs to
# read sysfs (which never blocks), but the regression risk remains for
# the other callers of BtrfsReplaceStatus (progress, recover); the
# `-1` contract is now pinned in the cmd-helper unit test
# btrfs_replace_status_includes_minus_one. This subtest is the
# end-to-end pair for the new sysfs path: cmd helper (probe_fsid) +
# Filesystem read + idle wiring against live tool output.
# This subtest is also the canonical live proof of the `braid idle` exit-1
# exclusive-operation branch (cmd_idle step 2); tests/cli/braid-idle.py points
# here for that path. Do not weaken the exit-1 / "device replace" assertions
# below without relocating that coverage.
#
# Scenario: replace is mid-flight (verified above). Operator's autosuspend
# daemon polls `braid idle`. The call must return within seconds and
# report a device-replace busy reason.
with subtest("braid idle returns promptly and detects in-flight replace via sysfs"):
    idle_exit, idle_out = machine.execute("timeout 5 braid idle 2>&1")
    print(f"=== braid idle during replace (exit {idle_exit}) ===")
    print(idle_out)
    assert idle_exit != 124, (
        "braid idle did not return within 5 s while a replace was in "
        "flight -- a blocking subprocess probe was likely re-introduced. "
        "Check cli/src/idle.rs and cli/src/cmd.rs."
    )
    assert idle_exit == 1, (
        f"braid idle should report busy (exit 1) during a replace, "
        f"got exit {idle_exit}: {idle_out}"
    )
    assert "device replace" in idle_out.lower(), (
        f"braid idle did not report device replace as the busy reason: {idle_out}"
    )

# --- Phase 4: Assert the inhibitor is held while the replace is in flight ---

with subtest("braid sleep inhibitor is held during replace"):
    mid = list_inhibitors()
    print(f"=== inhibitors mid-replace: {mid} ===")
    inh = find_braid_sleep_inhibitor(mid)
    assert inh is not None, (
        "no braid sleep inhibitor found while replace is in flight. "
        f"inhibitors: {mid}"
    )
    # Bonus locks on the exact shape we expect, so a regression in any
    # field (mode flip, who rename, what widening) fails loudly.
    assert inh["what"] == "sleep", f"expected what=sleep, got {inh!r}"
    assert inh["mode"] == "block", f"expected mode=block, got {inh!r}"
    assert "replace" in inh["why"], f"expected why mentioning replace, got {inh!r}"
    # Capture the inhibitor pid so Phase 6 can verify the entire process
    # group is torn down on release (not just the systemd-inhibit parent).
    inhibitor_pid = inh["pid"]
    dm_delay_deactivate(machine, "disk4")

# --- Phase 5: Wait for the replace to finish ---

with subtest("Wait for replace to finish"):
    # `btrfs replace status -1` prints "Started on <t1>, finished on <t2>"
    # in the FINISHED state — see reference/btrfs-progs/cmds/replace.c:460.
    machine.wait_until_succeeds(
        "btrfs replace status -1 /mnt/storage 2>&1 | grep -q 'finished on'",
        timeout=300,
    )
    print("=== final btrfs replace status ===")
    print(machine.succeed("btrfs replace status -1 /mnt/storage"))

# --- Phase 6: Assert the inhibitor is released after the replace ---
#
# logind releases the inhibitor when the systemd-inhibit child exits, which
# happens when the SleepInhibitor RAII guard in cmd_replace is dropped. Use
# wait_until_succeeds to allow a brief settle window — the release is
# typically immediate but should not be timing-fragile.

with subtest("braid sleep inhibitor is released after replace"):
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
            f"braid sleep inhibitor still held 30s after replace finished: {post}"
        )

    post = list_inhibitors()
    print(f"=== inhibitors post-replace: {post} ===")

with subtest("inhibitor process group is torn down (no leaked sh/sleep)"):
    # Regression test: SleepInhibitor::Drop must kill the entire process
    # group, not just the systemd-inhibit parent. Without process_group(0)
    # + kill(-pgid, SIGKILL), the supervised `sh -c '...; exec sleep
    # infinity'` child would survive the parent's death and accumulate as
    # an orphan reparented to init on every replace.
    #
    # SleepInhibitor spawns systemd-inhibit with process_group(0), so the
    # pgid equals the systemd-inhibit pid. After teardown, `pgrep -g <pgid>`
    # must find no live members. Allow a brief settle window since the
    # child reap is async with respect to the inhibitor release we asserted
    # above.
    def pgroup_empty():
        ret = machine.execute(f"pgrep -g {inhibitor_pid}")
        # pgrep exits non-zero (1) when no processes match.
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

with subtest("Pool integrity after replace"):
    machine.succeed("mountpoint -q /mnt/storage")
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed across replace: pre={payload_sha} post={post_sha}"
    )
    fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print("=== final btrfs filesystem show ===")
    print(fs_show)
    assert "MISSING" not in fs_show, (
        "phantom MISSING device after a clean (non-interrupted) replace — "
        "either the replace was interrupted or topology drifted:\n" + fs_show
    )
    assert "Total devices 3" in fs_show, (
        f"expected 3 devices after replace, got:\n{fs_show}"
    )
    # disk2 (the replace source) must be gone, disk4 (the target) must be present.
    assert "braid-disk4" in fs_show, f"replace target disk4 missing:\n{fs_show}"
    assert "braid-disk2" not in fs_show, f"replace source disk2 still present:\n{fs_show}"

machine.shutdown()
