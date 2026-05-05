# Repro: btrfs replace interrupted mid-flight by unclean VM crash
#
# Intent: Pin down end-to-end recovery from a `braid replace` interrupted
# by an unclean kill (qemu SIGKILL → kernel dies). After reboot, `braid
# recover` must read the post-resume on-disk topology and write a clean
# pool.json — no phantom MISSING devid, no DEGRADED status, no manual
# cleanup recipe.
#
# Why it exists: tests/cli/recover-replace-not-started.py covers crash before
# `btrfs replace start` runs, and tests/cli/recover-replace-completed.py
# covers crash after the replace completes. The in-flight crash window between
# them used to leave the pool in a known-broken state because the kernel's
# resume-on-mount path (btrfs_resume_dev_replace_async, called from mount
# when the on-disk dev_replace_item is in STARTED) commits the post-completion
# devid swap to disk correctly but does NOT update the in-memory
# btrfs_fs_devices for the mount session that triggered the resume. Without
# the recover-side fix, recover's probe_pool reads from that stale in-memory
# state and persists a snapshot containing a phantom MISSING devid 0 and
# both the source and target devices. cmd_recover now cycles the mount
# (umount + btrfs device scan --forget + remount) before probing so the
# kernel rebuilds fs_devices from the post-resume on-disk chunk tree. This
# test pins that fix in place.
#
# Scope: this test exercises ONLY the unclean-kill path. It does NOT exercise
# the v6.19+ freeze/signal cancellation path (try_to_freeze /
# fatal_signal_pending checks added inside the scrub worker loop) — that path
# requires a userspace process to be alive to observe the freeze, and an
# unclean kernel kill bypasses it entirely. A separate sibling test would be
# needed to exercise that path; see plans/wip/sharded-drifting-beaver-findings.md.
#
# Scenario: 3-disk RAID1 pool with a 400 MiB urandom payload. Operator starts
# `braid replace disk2 disk4` and the VM is forcibly crashed (qemu SIGKILL via
# machine.crash) once the kernel reports non-zero replace progress. After
# reboot, the test runs braid recover and asserts the recovered topology is
# clean: 3 devices (disk1, disk3, disk4), no MISSING, status intact, pool.json
# matches the post-replace target membership.

import shlex
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


def replace_cmd_bg(old, new):
    # Background the entire (passphrase | braid replace) pipeline so
    # machine.execute returns immediately. The subshell makes & apply to
    # the whole pipeline rather than just the tail braid invocation.
    return (
        f"(printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
        f"--passphrase-stdin --yes) > /tmp/replace.log 2>&1 &"
    )


# --- Phase 1: Build a 3-disk RAID1 pool and write a verifiable payload ---

with subtest("Build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Write urandom payload"):
    machine.succeed(
        "dd if=/dev/urandom of=/mnt/storage/payload bs=1M count=400 status=none"
    )
    machine.succeed("sync")
    payload_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    print(f"payload sha256: {payload_sha}")

# --- Phase 2: Capture pre-crash state into the transcript ---

with subtest("Capture pre-crash state"):
    print("=== uname -r ===")
    print(machine.succeed("uname -r"))
    print("=== btrfs filesystem show /mnt/storage (pre-replace) ===")
    print(machine.succeed("btrfs filesystem show /mnt/storage"))
    print("=== /var/lib/braid/pool.json (pre-replace) ===")
    print(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 3: Start replace asynchronously, wait for in-flight progress ---
#
# Drive the test from observed kernel state, not timing assumptions:
# poll until btrfs reports a non-zero replace progress percentage. Hard-fail
# if we never observe in-flight state, because that means the scenario didn't
# exercise an interrupt and the test has degraded to a no-op.

with subtest("Start replace and wait for in-flight progress"):
    machine.execute(replace_cmd_bg("disk2", "disk4"))

    saw_running = False
    last_status = ""
    for _ in range(400):
        ret = machine.execute("btrfs replace status -1 /mnt/storage 2>&1")
        last_status = ret[1]
        # Output looks like: "0.5% done, 0 write errs, 0 uncorr. read errs"
        # As soon as the kernel reports any non-zero progress (or even 0.0%
        # alongside "Started on"), the kernel-level operation is in-flight.
        if "Started on" in last_status or "% done" in last_status:
            saw_running = True
            break
        time.sleep(0.05)

    print("=== last btrfs replace status before crash ===")
    print(last_status)
    assert saw_running, (
        "Never observed btrfs replace in-flight — test cannot exercise the "
        "interrupted-replace scenario. Last status:\n" + last_status
    )

# --- Phase 4: Crash the VM mid-replace ---
#
# machine.crash() sends SIGKILL to qemu — the strongest available involuntary
# interruption, closer to a power loss than a signal or filesystem freeze.

with subtest("Crash VM mid-replace"):
    machine.crash()

# --- Phase 5: Reboot and capture post-crash state ---

with subtest("Boot back up"):
    machine.start()
    machine.wait_for_unit("multi-user.target")

with subtest("Capture post-crash mapper state"):
    for d in ["disk1", "disk2", "disk3", "disk4"]:
        ret = machine.execute(f"cryptsetup status braid-{d} 2>&1")
        print(f"=== cryptsetup status braid-{d} (exit {ret[0]}) ===")
        print(ret[1])

with subtest("Run braid unlock"):
    unlock_exit, unlock_out = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    print(f"=== braid unlock (exit {unlock_exit}) ===")
    print(unlock_out)
    # Safety floor: braid unlock must not panic. The exit code itself can
    # be non-zero (e.g. if a journal is detected and recovery is required),
    # but a Rust panic is always a bug.
    assert "panicked at" not in unlock_out, (
        f"braid unlock panicked:\n{unlock_out}"
    )
    assert "RUST_BACKTRACE" not in unlock_out, (
        f"braid unlock emitted a backtrace:\n{unlock_out}"
    )

with subtest("Capture post-crash pool and journal state"):
    pool_json_ret = machine.execute("cat /var/lib/braid/pool.json 2>&1")
    print(f"=== /var/lib/braid/pool.json (exit {pool_json_ret[0]}) ===")
    print(pool_json_ret[1])
    pool_json_post_crash = pool_json_ret[1]

    journal_ret = machine.execute(
        "test -f /var/lib/braid/pending-op.json "
        "&& cat /var/lib/braid/pending-op.json "
        "|| echo NO_JOURNAL"
    )
    print(f"=== /var/lib/braid/pending-op.json (exit {journal_ret[0]}) ===")
    print(journal_ret[1])
    journal_present = "NO_JOURNAL" not in journal_ret[1]

with subtest("Run braid recover"):
    assert journal_present, (
        "Expected pending-op.json to survive the crash, but it does not exist. "
        "Either braid is clearing the journal too eagerly, or the crash happened "
        "before the journal was written."
    )
    recover_exit, recover_out = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin 2>&1"
    )
    print(f"=== braid recover (exit {recover_exit}) ===")
    print(recover_out)
    assert "panicked at" not in recover_out, (
        f"braid recover panicked:\n{recover_out}"
    )
    assert "RUST_BACKTRACE" not in recover_out, (
        f"braid recover emitted a backtrace:\n{recover_out}"
    )

with subtest("Capture final state"):
    final_fs_show_ret = machine.execute("btrfs filesystem show /mnt/storage 2>&1")
    print(f"=== final btrfs filesystem show (exit {final_fs_show_ret[0]}) ===")
    print(final_fs_show_ret[1])
    final_fs_show = final_fs_show_ret[1]

    final_braid_status_ret = machine.execute("braid status 2>&1")
    print(f"=== final braid status (exit {final_braid_status_ret[0]}) ===")
    print(final_braid_status_ret[1])
    final_braid_status = final_braid_status_ret[1]

# --- Phase 6: Safety-floor assertions ---
#
# These four invariants must hold regardless of how btrfs handles the
# interruption. They are the only hard asserts in this first pass; everything
# else above is captured for the findings note.

with subtest("Safety floor: pool is mounted"):
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("Safety floor: payload sha256 matches"):
    post_sha = machine.succeed("sha256sum /mnt/storage/payload").split()[0]
    assert post_sha == payload_sha, (
        f"payload sha256 changed: pre={payload_sha} post={post_sha}"
    )

with subtest("Safety floor: at least one of disk2 or disk4 in live pool"):
    fs_show_final = machine.succeed("btrfs filesystem show /mnt/storage")
    has_source = "braid-disk2" in fs_show_final
    has_target = "braid-disk4" in fs_show_final
    assert has_source or has_target, (
        "Both replace source (disk2) and target (disk4) are missing from the "
        "live pool — silent loss of one side of the replace.\n" + fs_show_final
    )

# --- Phase 7: Observation-lock assertions ---
#
# These assertions pin the FIXED behavior end-to-end: braid recover handles
# the kernel-resume-on-mount staleness via its remount cycle (umount +
# scan --forget + remount) and writes a clean pool.json. Any drift —
# kernel resume semantics changing, the recover-side cycle being removed,
# the journal handling shifting — fails the test loudly so the regression
# is visible in CI. See plans/wip/sharded-drifting-beaver-findings.md for
# the full investigation that motivated the fix.

with subtest("Observation lock: braid unlock refuses with journal-detected error"):
    # Locks in cmd_unlock's interrupted-operation guard.
    assert unlock_exit != 0, (
        f"braid unlock should refuse when a journal exists, got exit 0:\n{unlock_out}"
    )
    assert "interrupted operation detected" in unlock_out, (
        f"braid unlock did not emit the journal-detected error:\n{unlock_out}"
    )

with subtest("Observation lock: pool.json unchanged across crash"):
    # Locks in that braid does not mutate pool.json mid-operation, so the
    # post-crash file still describes the pre-replace topology.
    assert '"disk2"' in pool_json_post_crash, (
        f"pool.json post-crash is missing disk2 (the replace source); "
        f"braid mutated it before the crash:\n{pool_json_post_crash}"
    )
    assert '"disk4"' not in pool_json_post_crash, (
        f"pool.json post-crash already contains disk4 (the replace target); "
        f"braid mutated it before the replace finished:\n{pool_json_post_crash}"
    )

with subtest("Observation lock: kernel resumed and finished the replace on mount"):
    # The kernel resume-on-mount path (btrfs_resume_dev_replace_async) restarts
    # the in-progress replace from the on-disk cursor (effectively 0% in this
    # scenario) and runs it to completion synchronously during the recover
    # mount. After braid recover (and its remount cycle), disk4 must be the
    # surviving target visible in btrfs filesystem show. If this assert
    # flips, either the kernel resume path got rewritten or braid stopped
    # triggering it from recover — both worth investigating.
    assert "braid-disk4" in final_fs_show, (
        "Kernel did not resume the replace on mount — disk4 is missing from "
        "the live pool after recovery. The resume-on-mount code path may have "
        "changed; revisit plans/wip/sharded-drifting-beaver-findings.md.\n"
        + final_fs_show
    )

with subtest("Observation lock: post-recovery topology is clean (3 devices, no MISSING)"):
    # Pins the recover fix: cmd_recover's remount cycle drops the cached
    # in-memory btrfs_fs_devices left by the kernel resume worker, so the
    # second mount reads the post-completion swap from disk. The result is
    # a clean three-device topology with disk4 in disk2's old devid 2 slot.
    # If this assert fires, the recover-side fix has regressed (or the
    # kernel resume's on-disk handling changed).
    assert "MISSING" not in final_fs_show, (
        "phantom MISSING entry survived braid recover — the remount cycle "
        "in cmd_recover must have regressed (cli/src/recover.rs). Without "
        "it, the kernel's stale in-memory fs_devices for the original mount "
        "session is what probe_pool reads.\n" + final_fs_show
    )
    assert "Total devices 3" in final_fs_show, (
        "post-recovery topology should have exactly 3 devices (disk1, disk3, "
        "disk4) but does not. Either the recover fix regressed or the kernel "
        "resume completed unexpectedly.\n" + final_fs_show
    )
    # Source disk2 must be evicted; target disk4 must be present.
    assert "braid-disk2" not in final_fs_show, (
        "replace source disk2 still appears in the live pool — the kernel's "
        "post-completion swap was not picked up by braid recover.\n"
        + final_fs_show
    )

with subtest("Observation lock: braid status reports intact after recovery"):
    # No phantom MISSING means braid status reports a clean pool. If this
    # asserts fires, the recover fix has regressed.
    assert "DEGRADED" not in final_braid_status, (
        "braid status reports DEGRADED after recovery from an interrupted "
        "replace — the recover-side remount cycle has regressed. Revisit "
        "plans/wip/sharded-drifting-beaver-findings.md.\n"
        + final_braid_status
    )

with subtest("Observation lock: braid recover succeeds with replace-completed guidance"):
    # The recover fix means the live pool matches the journal's
    # target_membership exactly, so recovery_guidance picks the
    # replace-completed branch.
    assert recover_exit == 0, (
        f"braid recover failed (exit {recover_exit}). With the remount-cycle "
        f"fix in place, recover should succeed cleanly on this scenario.\n"
        f"{recover_out}"
    )
    assert "replace completed" in recover_out, (
        "braid recover did not emit the 'replace completed' guidance — the "
        "recovered membership does not match journal.target_membership. "
        "Either the cycle wrote a stale snapshot (recover regression) or "
        "the kernel resume did not finish on this run.\n" + recover_out
    )
    assert "membership does not match" not in recover_out, (
        "braid recover still prints 'membership does not match' — the cycle "
        "is reading stale state. Revisit cli/src/recover.rs and the findings "
        "note.\n" + recover_out
    )
    # Journal should have been cleared after recovery succeeded.
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("Observation lock: pool.json reflects the post-replace target membership"):
    # The recovered pool.json must have exactly disk1, disk3, disk4 — the
    # post-replace target. If disk2 reappears, the cycle picked up stale
    # state; if disk4 is missing, the kernel resume did not run.
    final_pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    print(f"=== final pool.json ===\n{final_pool_json}")
    assert '"disk1"' in final_pool_json, (
        f"pool.json missing disk1 after recovery:\n{final_pool_json}"
    )
    assert '"disk3"' in final_pool_json, (
        f"pool.json missing disk3 after recovery:\n{final_pool_json}"
    )
    assert '"disk4"' in final_pool_json, (
        f"pool.json missing disk4 after recovery — kernel resume did not "
        f"finish or recover wrote a stale snapshot:\n{final_pool_json}"
    )
    assert '"disk2"' not in final_pool_json, (
        f"pool.json still contains the evicted disk2 — recover wrote a "
        f"stale in-memory snapshot. cmd_recover's remount cycle has "
        f"regressed.\n{final_pool_json}"
    )

with subtest("Observation lock: subsequent lock/unlock cycle stays clean"):
    # A clean recover should leave the pool in a state where a normal
    # (non-degraded) braid lock + braid unlock cycle works without
    # re-introducing any MISSING entries. This is the strongest end-to-end
    # check: it proves the on-disk state matches what pool.json now claims.
    machine.succeed("braid lock")
    cycle_unlock_exit, cycle_unlock_out = machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin 2>&1"
    )
    print(f"=== braid unlock after lock cycle (exit {cycle_unlock_exit}) ===")
    print(cycle_unlock_out)
    assert cycle_unlock_exit == 0, (
        "braid unlock (no --allow-degraded) failed after a clean recover — "
        f"recover may have left pool.json out of sync with the live pool.\n"
        f"{cycle_unlock_out}"
    )
    cycle_fs_show = machine.succeed("btrfs filesystem show /mnt/storage")
    print(f"=== fs show after lock/unlock cycle ===\n{cycle_fs_show}")
    assert "MISSING" not in cycle_fs_show, (
        f"MISSING re-appeared after a lock/unlock cycle:\n{cycle_fs_show}"
    )
    assert "Total devices 3" in cycle_fs_show, (
        f"pool no longer has 3 devices after a lock/unlock cycle:\n{cycle_fs_show}"
    )

machine.shutdown()
