# Test: systemd-lifecycle
#
# Intent: Verify the systemd state machine that manages pool lifecycle --
# braid-pool.target as entry point, braid-online.service as lifecycle owner,
# and Rust dispatch synchronization between braid unlock/lock/add and
# braid-online.service activation state.
#
# Why it exists: The lifecycle model's systemd side has two moving parts
# (target, services) that must stay synchronized, while Rust dispatch owns
# post-command online-state transitions and pool-lock serialization. Existing
# tests cover CLI behavior (unlock, lock) and auto-unlock, but don't directly
# verify systemd unit state transitions. A broken dependency or dispatch
# regression could leave braid-online.service out of sync with actual pool
# state, causing silent failure of automatic locking on shutdown.
# See docs/design/decisions/026-pool-lock-rust-owned.md.
#
# Scenario: 2-disk RAID1 pool pre-created by initrd fixture (disk1, disk2),
# plus spare disks (disk3 for the add test, disk4+disk5 for the concurrent-add
# test). Tests exercise:
# (1) systemctl start braid-pool.target brings pool online and activates
#     braid-online.service,
# (2) systemctl stop braid-online.service unmounts pool and closes LUKS,
# (3) braid unlock/lock via Rust dispatch correctly activates/deactivates
#     braid-online.service,
# (4) braid add via Rust dispatch activates braid-online.service,
# (5) Rust dispatch prints warning but still succeeds when braid-online.service
#     cannot be activated,
# (6) two concurrent braid add invocations: the Rust-owned non-blocking
#     flock lets exactly one complete and rejects the other with the
#     contention error, leaving pool.json corruption-free,
# (7) actual VM shutdown/reboot runs ExecStop=braid lock to completion.

import json


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

# --- Subtest 1: Precondition — pool is offline after boot ---

with subtest("Precondition: pool offline after boot"):
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# --- Subtest 2: braid-online has generous stop timeout ---

with subtest("braid-online has generous stop timeout"):
    timeout = machine.succeed(
        "systemctl show braid-online.service -p TimeoutStopUSec --value"
    ).strip()
    assert timeout == "5min", "Expected TimeoutStopUSec=5min, got {}".format(timeout)

# --- Subtest 3: Direct start skipped when pool unmounted ---

with subtest("Direct start of braid-online.service skipped when pool unmounted"):
    # ConditionPathIsMountPoint causes systemd to skip activation (exit 0)
    # when the mount point isn't mounted. Verify the unit stays inactive.
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed("systemctl start braid-online.service")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 3: braid-pool.target round-trip ---

with subtest("braid-pool.target brings pool online"):
    machine.succeed("systemctl start braid-pool.target")

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("test -e /dev/mapper/braid-disk1")
    machine.succeed("test -e /dev/mapper/braid-disk2")

# --- Subtest 3: Manual stop via lifecycle owner ---

with subtest("Stopping braid-online.service locks pool"):
    # ExecStop runs `braid lock` which unmounts and closes LUKS.
    # The post-lock `systemctl stop braid-online` from `mark_offline` is a no-op
    # because the service is already deactivating — harmless.
    machine.succeed("systemctl stop braid-online.service")

    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 4: Re-unlock via braid-pool.target after lock ---

with subtest("braid-pool.target re-unlock after lock"):
    # After stopping braid-online (which ran braid lock), the pool is offline.
    # braid-unlock.service should be inactive (no RemainAfterExit), so
    # re-starting braid-pool.target must trigger a fresh unlock cycle.
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    machine.succeed("systemctl start braid-pool.target")

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

    # Tear down via lifecycle owner (systemd path, not CLI path).
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("systemctl is-active braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")

# --- Subtest 5: CLI dispatch synchronization (unlock) ---

with subtest("braid unlock activates braid-online.service"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")

# --- Subtest 5: CLI dispatch synchronization (lock) ---

with subtest("braid lock deactivates braid-online.service"):
    machine.succeed("braid lock")

    machine.fail("systemctl is-active braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# --- Subtest 6: Concurrent unlocks: one wins, the other fast-fails or sees mounted ---

with subtest("Concurrent unlocks: one wins, the other fast-fails or sees mounted"):
    # Intent: Two concurrent `braid unlock` invocations must not race
    # into cryptsetup open on the same devices. The Rust-owned
    # non-blocking flock on /run/braid-pool.lock enforces mutual
    # exclusion — exactly one process unlocks the pool. The loser
    # either fast-fails on contention (lost the flock race) or, if it
    # acquired the lock sequentially after the winner released, sees
    # the pool already mounted and exits cleanly.
    #
    # Why it exists: braid-auto-unlock and braid-unlock can both pass
    # their ConditionPathIsMountPoint gate before either mounts the
    # pool. Without the flock, the second cryptsetup open fails with
    # EBUSY, leaving partial LUKS state requiring manual cleanup.
    #
    # Scenario: User SSHs in and runs `systemctl start braid-pool.target`
    # while braid-auto-unlock is still in-flight at boot.
    machine.fail("mountpoint -q /mnt/storage")

    # Launch two concurrent unlocks, capture per-process exit codes.
    # NixOS test driver wraps every command with `set -euo pipefail` --
    # nixpkgs b51242d7, nixos/lib/test-driver/src/test_driver/machine/__init__.py
    # (fn `QemuMachine._execute`): `command = f"set -euo pipefail; {command}"` --
    # so a bare
    # `wait $pid_loser` returning non-zero would abort the chain
    # before the exit-file writes. The `wait $pid || ec=$?` idiom
    # consumes the non-zero return into a variable so errexit does
    # not fire.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin "
        f">/tmp/unlock-a 2>&1 & pid_a=$! ; "
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin "
        f">/tmp/unlock-b 2>&1 & pid_b=$! ; "
        f"ec_a=0 ; wait $pid_a || ec_a=$? ; echo $ec_a > /tmp/unlock-exit-a ; "
        f"ec_b=0 ; wait $pid_b || ec_b=$? ; echo $ec_b > /tmp/unlock-exit-b"
    )

    exit_a = int(machine.succeed("cat /tmp/unlock-exit-a").strip())
    exit_b = int(machine.succeed("cat /tmp/unlock-exit-b").strip())
    out_a = machine.succeed("cat /tmp/unlock-a")
    out_b = machine.succeed("cat /tmp/unlock-b")

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

    def loser_ok(exit_code: int, out: str) -> bool:
        # Loser acceptable outcomes:
        #   exit 0 + "pool already mounted" — sequential winner: lost
        #     the flock race after the winner released, then re-checked
        #     and bailed cleanly.
        #   exit 1 + contention message — concurrent loser: lost the
        #     flock race while the winner still held it.
        if exit_code == 0 and "pool already mounted" in out:
            return True
        if exit_code == 1 and "another braid operation is already in progress" in out:
            return True
        return False

    a_winner = exit_a == 0 and "pool already mounted" not in out_a
    b_winner = exit_b == 0 and "pool already mounted" not in out_b
    assert a_winner ^ b_winner, (
        "Expected exactly one winner.\n"
        "A: exit={} out={}\nB: exit={} out={}".format(exit_a, out_a, exit_b, out_b)
    )

    if a_winner:
        assert loser_ok(exit_b, out_b), (
            "Loser B has unexpected outcome: exit={} out={}".format(exit_b, out_b)
        )
    else:
        assert loser_ok(exit_a, out_a), (
            "Loser A has unexpected outcome: exit={} out={}".format(exit_a, out_a)
        )

    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 7: braid add activates braid-online.service ---

with subtest("braid add activates braid-online.service"):
    # Pool is offline from subtest 5. Manually open existing LUKS mappers
    # and mount pool, bypassing Rust dispatch so `mark_online` does not run.
    # This isolates the `add` activation path from the `unlock` path.
    machine.succeed(
        f"printf '%s\\n' {pq} | cryptsetup open /dev/disk/by-id/virtio-disk1 braid-disk1"
    )
    machine.succeed(
        f"printf '%s\\n' {pq} | cryptsetup open /dev/disk/by-id/virtio-disk2 braid-disk2"
    )
    machine.succeed(
        "btrfs device scan /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
    )
    machine.succeed("mount /dev/mapper/braid-disk1 /mnt/storage")

    # Pool is mounted but braid-online is still inactive (`mark_online` did not run).
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    # Add a 3rd disk through Rust dispatch -- `mark_online` must activate braid-online.
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin --yes"
    )

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 7: Negative path -- braid-online activation failure ---

with subtest("Rust dispatch warns but succeeds when braid-online.service fails"):
    # Override ExecStart with /bin/false via a runtime drop-in so that
    # `systemctl start braid-online.service` fails.  This triggers the
    # Rust dispatch's WARNING code path.  (systemctl mask does not work reliably
    # on NixOS-managed units whose symlinks live in /etc/systemd/system/.)
    machine.succeed(
        "mkdir -p /run/systemd/system/braid-online.service.d && "
        "printf '[Service]\\nExecStart=\\nExecStart=/bin/false\\n' "
        "> /run/systemd/system/braid-online.service.d/99-fail.conf"
    )
    machine.succeed("systemctl daemon-reload")

    # Verify the override makes the service fail. Temporarily satisfy
    # ConditionPathIsMountPoint with a tmpfs so ExecStart=/bin/false is
    # actually reached — proving the override works independently of the
    # condition guard.
    machine.succeed("mount -t tmpfs tmpfs /mnt/storage")
    machine.fail("systemctl start braid-online.service")
    machine.succeed("umount /mnt/storage")

    # Run unlock, capturing output to a file for reliable assertion.
    # machine.execute() output capture can be inconsistent with pipes.
    machine.execute(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin "
        f">/tmp/unlock-out 2>&1; echo $? >/tmp/unlock-exit"
    )
    exit_code = int(machine.succeed("cat /tmp/unlock-exit").strip())
    output = machine.succeed("cat /tmp/unlock-out")
    print(f"Unlock output:\n{output}")
    assert exit_code == 0, f"Expected exit 0, got {exit_code}: {output}"

    # Pool must still be mounted and usable despite the warning.
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    assert "WARNING" in output and "braid-online" in output, (
        f"Expected warning about braid-online.service, got: {output}"
    )

    # Cleanup
    machine.succeed("braid lock")
    machine.succeed("rm -rf /run/systemd/system/braid-online.service.d")
    machine.succeed("systemctl daemon-reload")

# --- Subtest 8: braid recover activates braid-online.service ---

with subtest("braid recover activates braid-online.service"):
    # Pool is offline from previous cleanup.
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    # Inject a pending-op.json to enter recovery mode.
    # The pool now has 3 disks (after subtest 6 added disk3). Use a
    # PostAddBalanceRaid1 journal whose membership already matches the live
    # pool so recover only mounts, reconciles pool.json, and clears recovery
    # mode through Rust dispatch.
    pool_json_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool_membership = json.loads(pool_json_raw)
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "phase": "PostAddBalanceRaid1",
            "targets": {},
        },
        "pre_membership": pool_membership,
        "target_membership": pool_membership,
    }
    journal_json = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_json}\n"
        f"JOURNAL_EOF"
    )

    # Recover through Rust dispatch -- should mount and activate braid-online.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("test -f /var/lib/braid/pending-op.json")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 9: Concurrent adds reject the loser cleanly ---

with subtest("Concurrent adds reject the loser cleanly"):
    # Intent: When two `braid add` invocations race, the Rust-owned
    # non-blocking flock must let exactly one win. The loser fails
    # fast with the contention message; pool.json reflects only the
    # winner's disk, btrfs sees only the winner's device, and no
    # residual pending-op.json is left from the rejected attempt.
    #
    # Why it exists: braid does not queue pool operations. The
    # earlier serialization contract ("both adds complete") was
    # incompatible with non-blocking flock. This test guards the
    # new contract: rejection without state corruption.
    #
    # Scenario: A script bug launches two `braid add` commands at
    # the same time. One succeeds, the other reports the contention
    # error and exits 1; the operator sees the error and retries
    # the rejected disk after the first completes.

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    # Launch two concurrent adds, capture per-process exit codes.
    # See subtest 6 above for why we use `wait $pid || ec=$?` —
    # NixOS test driver wraps commands with `set -e`, so a bare
    # `wait $pid_loser` would abort the chain before exit-file writes.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk4=/dev/disk/by-id/virtio-disk4 --passphrase-stdin --yes "
        f">/tmp/add-a 2>&1 & pid_a=$! ; "
        f"printf '%s\\n' {pq} | braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 disk5=/dev/disk/by-id/virtio-disk5 --passphrase-stdin --yes "
        f">/tmp/add-b 2>&1 & pid_b=$! ; "
        f"ec_a=0 ; wait $pid_a || ec_a=$? ; echo $ec_a > /tmp/exit-a ; "
        f"ec_b=0 ; wait $pid_b || ec_b=$? ; echo $ec_b > /tmp/exit-b"
    )

    exit_a = int(machine.succeed("cat /tmp/exit-a").strip())
    exit_b = int(machine.succeed("cat /tmp/exit-b").strip())
    out_a = machine.succeed("cat /tmp/add-a")
    out_b = machine.succeed("cat /tmp/add-b")

    # Exactly one must succeed and one must fail.
    assert (exit_a == 0) ^ (exit_b == 0), (
        "Expected exactly one winner.\n"
        "A: exit={} out={}\nB: exit={} out={}".format(exit_a, out_a, exit_b, out_b)
    )

    if exit_a == 0:
        winner_disk = "disk4"
        loser_disk = "disk5"
        loser_exit = exit_b
        loser_out = out_b
    else:
        winner_disk = "disk5"
        loser_disk = "disk4"
        loser_exit = exit_a
        loser_out = out_a

    assert loser_exit == 1, (
        "Loser exited {}, expected 1: {}".format(loser_exit, loser_out)
    )
    assert "another braid operation is already in progress" in loser_out, (
        "Loser missing contention message: {}".format(loser_out)
    )

    # pool.json must contain disks 1-3 plus exactly the winner.
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    expected = {"disk1", "disk2", "disk3", winner_disk}
    actual = member_names(pool)
    assert actual == expected, (
        "pool.json mismatch.\nexpected: {}\nactual:   {}".format(expected, actual)
    )
    assert loser_disk not in actual, (
        "Loser disk {} leaked into pool.json: {}".format(loser_disk, actual)
    )

    # btrfs must show 4 devices, not 5.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 4, (
        "Expected 4 btrfs devices, got {}:\n{}".format(devid_count, fi_show)
    )

    # Rust dispatch acquires the pool lock BEFORE it writes the
    # journal, so the rejected attempt must leave no pending-op.json.
    machine.fail("test -f /var/lib/braid/pending-op.json")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 10: Shutdown runs ExecStop=braid lock ---
#
# Subtests 3/5 test manual stop, but systemd's shutdown ordering differs.
# DefaultDependencies adds Conflicts=shutdown.target + timeout enforcement.
# ExecStop could be skipped or killed if ordering is wrong. Post-reboot
# state (mappers gone, mount gone) proves nothing — a reboot clears those
# regardless. The journal is the real proof.

# Setup: unlock pool, write canary, trigger real shutdown.
machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
machine.succeed("systemctl is-active braid-online.service")
machine.succeed("echo 'shutdown-canary' > /mnt/storage/canary.txt")
machine.succeed("sync")

machine.shutdown()
machine.start()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("ExecStop=braid lock completes during shutdown"):
    # PRIMARY: previous boot's journal proves ExecStop ran to completion.
    # "Stopped Braid storage pool online." means systemd saw clean exit.
    svc_log = machine.succeed(
        "journalctl -b -1 -u braid-online.service --no-pager"
    )
    assert "Stopped Braid storage pool online" in svc_log, (
        f"ExecStop did not complete during shutdown. Journal:\n{svc_log}"
    )
    assert "timed out" not in svc_log.lower(), (
        f"braid-online.service was killed by timeout. Journal:\n{svc_log}"
    )

    # SECONDARY: canary file survives — data integrity after clean unmount.
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    content = machine.succeed("cat /mnt/storage/canary.txt").strip()
    assert content == "shutdown-canary", (
        f"Expected 'shutdown-canary', got '{content}'"
    )

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

machine.shutdown()
