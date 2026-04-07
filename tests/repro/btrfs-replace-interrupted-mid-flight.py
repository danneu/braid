# Repro: btrfs replace interrupted mid-flight by unclean VM crash
#
# Intent: Pin down what happens to a btrfs RAID1 pool when a `braid replace`
# operation is interrupted by an unclean kill (qemu SIGKILL → kernel dies),
# on the pinned NixOS toolchain. Captures post-crash state from btrfs,
# cryptsetup, and braid into the test transcript and asserts a small set of
# safety-floor invariants plus observation locks that pin in the current
# kernel-resume + braid-recover behavior.
#
# Why it exists: tests/cli/recover-replace-not-started.py covers crash before
# `btrfs replace start` runs, and tests/cli/recover-replace-completed.py
# covers crash after the replace completes. The in-flight crash window between
# them is uncovered. The kernel's resume-on-mount path
# (btrfs_resume_dev_replace_async, called from mount when the on-disk
# dev_replace_item is in STARTED) re-enters the scrub-copy loop from the saved
# cursor when the original mount died unclean — and produces a non-standard
# topology when it finishes mid-recover, which is what this test pins down.
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
# reboot, the test unlocks the pool and captures every relevant state for
# inspection.

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
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def replace_cmd_bg(old, new):
    # Background the entire (passphrase | braid replace) pipeline so
    # machine.execute returns immediately. The subshell makes & apply to
    # the whole pipeline rather than just the tail braid invocation.
    return (
        f"(printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new}=/dev/disk/by-id/virtio-{new} "
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
# These assertions encode the CURRENT observed behavior of the kernel
# resume-on-mount path (btrfs_resume_dev_replace_async, unchanged across the
# kernels braid currently targets) and braid recover, on the pinned NixOS
# stable lane. See plans/wip/sharded-drifting-beaver-findings.md for the full
# transcript and analysis. They are not statements of correctness: the
# locked-in behavior includes a known-broken degraded-pool outcome that a
# follow-up plan will address. The asserts exist so that any drift in either
# the kernel resume semantics or the braid recover flow fails the test loudly,
# which is the signal to revisit the follow-up plan.

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
    # mount. After braid recover, disk4 must be visible in btrfs filesystem
    # show as a participating device. This assert flipping would mean either
    # the kernel resume path got rewritten or braid stopped triggering it from
    # recover — both worth investigating.
    assert "braid-disk4" in final_fs_show, (
        "Kernel did not resume the replace on mount — disk4 is missing from "
        "the live pool after recovery. The resume-on-mount code path may have "
        "changed; revisit plans/wip/sharded-drifting-beaver-findings.md.\n"
        + final_fs_show
    )

with subtest("Observation lock: post-recovery topology has phantom MISSING device"):
    # Locks in the broken outcome: the resumed replace finishes the data
    # copy but does not perform the post-completion devid swap, leaving the
    # pool with five device entries — disk2 still as devid 2, disk4 added at
    # devid 0, and a phantom MISSING devid 0. This is the bug a follow-up
    # plan needs to address.
    assert "MISSING" in final_fs_show, (
        "Expected a phantom MISSING device in the post-recovery topology — "
        "the broken outcome has gone away. This may be a kernel fix to the "
        "resume-on-mount swap path, or a braid recover fix landing; revisit "
        "plans/wip/sharded-drifting-beaver-findings.md.\n"
        + final_fs_show
    )
    assert "Total devices 5" in final_fs_show, (
        "Expected 5 device entries (3 originals + disk4 + phantom MISSING) in "
        "the post-recovery topology. Topology has shifted; revisit "
        "plans/wip/sharded-drifting-beaver-findings.md.\n" + final_fs_show
    )

with subtest("Observation lock: braid status reports DEGRADED after recovery"):
    # The phantom MISSING device causes braid status to report degraded,
    # even though all four physical disks are present.
    assert "DEGRADED" in final_braid_status, (
        "braid status no longer reports DEGRADED after recovery from an "
        "interrupted replace. This may be a kernel fix or a braid recover "
        "fix landing; revisit plans/wip/sharded-drifting-beaver-findings.md.\n"
        + final_braid_status
    )

with subtest("Observation lock: braid recover succeeds despite the broken topology"):
    # Locks in the current behavior: braid recover prints a `note:` about
    # the membership mismatch but exits 0 and clears the journal anyway.
    # The follow-up plan is expected to escalate this to a hard error;
    # when that lands, this assert flips and the test reminds us to update.
    assert recover_exit == 0, (
        f"braid recover exit code changed from 0 to {recover_exit}. "
        f"This is likely the follow-up fix landing — revisit "
        f"plans/wip/sharded-drifting-beaver-findings.md and update the test.\n"
        f"{recover_out}"
    )
    assert "membership does not match" in recover_out, (
        "braid recover no longer prints the 'membership does not match' note. "
        "Either the topology is now clean (good) or the message wording "
        "changed (update the assert). Revisit findings note.\n" + recover_out
    )
    # Journal should have been cleared after recovery succeeded.
    machine.fail("test -f /var/lib/braid/pending-op.json")

machine.shutdown()
