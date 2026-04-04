# Test: systemd-lifecycle
#
# Intent: Verify the systemd state machine that manages pool lifecycle —
# braid-pool.target as entry point, braid-online.service as lifecycle owner,
# and the CLI wrapper synchronization between braid unlock/lock/add and
# braid-online.service activation state.
#
# Why it exists: The lifecycle model has three moving parts (target, services,
# wrapper script) that must stay synchronized. Existing tests cover CLI
# behavior (unlock, lock) and auto-unlock, but don't directly verify systemd
# unit state transitions. A broken wrapper or misconfigured dependency could
# leave braid-online.service out of sync with actual pool state, causing
# silent failure of automatic locking on shutdown.
#
# Scenario: 2-disk RAID1 pool pre-created by initrd fixture (disk1, disk2),
# plus spare disks (disk3 for the add test, disk4+disk5 for the concurrent-add
# test). Tests exercise:
# (1) systemctl start braid-pool.target brings pool online and activates
#     braid-online.service,
# (2) systemctl stop braid-online.service unmounts pool and closes LUKS,
# (3) braid unlock/lock via CLI wrapper correctly activates/deactivates
#     braid-online.service,
# (4) braid add via CLI wrapper activates braid-online.service,
# (5) wrapper prints warning but still succeeds when braid-online.service
#     cannot be activated,
# (6) two concurrent braid add invocations serialize via the wrapper's flock
#     and both complete successfully,
# (7) actual VM shutdown/reboot runs ExecStop=braid lock to completion.

import json
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
    # The wrapper's post-lock `systemctl stop braid-online` is a no-op
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

# --- Subtest 5: CLI wrapper synchronization (unlock) ---

with subtest("braid unlock activates braid-online.service"):
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")

# --- Subtest 5: CLI wrapper synchronization (lock) ---

with subtest("braid lock deactivates braid-online.service"):
    machine.succeed("braid lock")

    machine.fail("systemctl is-active braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("test -e /dev/mapper/braid-disk1")
    machine.fail("test -e /dev/mapper/braid-disk2")

# --- Subtest 6: Concurrent unlock attempts serialize via flock ---

with subtest("Concurrent unlock attempts serialize via flock"):
    # Intent: Two concurrent `braid unlock` invocations must not race into
    # cryptsetup open on the same devices. The wrapper's flock on
    # /run/braid-pool.lock serializes them — the winner unlocks, the loser
    # acquires the lock, re-checks mountpoint, and exits cleanly.
    #
    # Why it exists: braid-auto-unlock and braid-unlock can both pass their
    # ConditionPathIsMountPoint gate before either mounts the pool. Without
    # the flock, the second cryptsetup open fails with EBUSY, leaving
    # partial LUKS state requiring manual cleanup.
    #
    # Scenario: User SSHs in and runs `systemctl start braid-pool.target`
    # while braid-auto-unlock is still in-flight at boot.
    machine.fail("mountpoint -q /mnt/storage")

    # Launch two concurrent unlock attempts through the wrapper.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin >/tmp/unlock-a 2>&1 & "
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin >/tmp/unlock-b 2>&1 & "
        f"wait"
    )

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

    out_a = machine.succeed("cat /tmp/unlock-a")
    out_b = machine.succeed("cat /tmp/unlock-b")
    assert "pool already mounted" in out_a or "pool already mounted" in out_b, (
        f"Expected one 'pool already mounted' message.\nA: {out_a}\nB: {out_b}"
    )

    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 7: braid add activates braid-online.service ---

with subtest("braid add activates braid-online.service"):
    # Pool is offline from subtest 5. Manually open existing LUKS mappers
    # and mount pool, bypassing the wrapper so braid-online stays inactive.
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

    # Pool is mounted but braid-online is still inactive (wrapper didn't run).
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    # Add a 3rd disk through the wrapper — this must activate braid-online.
    machine.succeed(
        f"printf '%s\\n' {pq} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin --yes"
    )

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 7: Negative path — wrapper activation failure ---

with subtest("Wrapper warns but succeeds when braid-online.service fails"):
    # Override ExecStart with /bin/false via a runtime drop-in so that
    # `systemctl start braid-online.service` fails.  This triggers the
    # wrapper's WARNING code path.  (systemctl mask does not work reliably
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
    print(f"Wrapper output:\n{output}")
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
    # The pool now has 3 disks (after subtest 6 added disk3). Build the
    # journal to match: pre_membership has all 3 actual pool members so
    # recover opens the right LUKS devices.
    pool_json_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool_membership = json.loads(pool_json_raw)
    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Add",
            "disks": {"disk99": "/dev/disk/by-id/virtio-disk99"},
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

    # Recover through the wrapper — should mount and activate braid-online.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin"
    )

    machine.succeed("systemctl is-active braid-online.service")
    machine.succeed("mountpoint -q /mnt/storage")
    machine.fail("test -f /var/lib/braid/pending-op.json")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

# --- Subtest 9: Concurrent add attempts serialize via flock ---

with subtest("Concurrent add attempts serialize via flock"):
    # Intent: Two concurrent `braid add` invocations must serialize via the
    # wrapper's flock — the winner adds its disk, then the loser acquires the
    # lock, reads the updated pool.json, and adds its own disk.
    #
    # Why it exists: The unlock flock test (subtest 6) covers the early-exit
    # re-check path. The add path is structurally different — the loser runs
    # the full add command after acquiring the lock. Without a test, a
    # regression that breaks flock for add would let concurrent adds race on
    # pool.json: the second write could lose the first's disk entry.
    #
    # Scenario: A script bug launches two `braid add` commands at the same
    # time. The flock serializes them so both complete and pool.json reflects
    # all disks.

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    # Launch two concurrent adds, capture PIDs for independent exit checks.
    machine.succeed(
        f"printf '%s\\n' {pq} | BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk4=/dev/disk/by-id/virtio-disk4 --passphrase-stdin --yes "
        f">/tmp/add-a 2>&1 & pid_a=$! ; "
        f"printf '%s\\n' {pq} | BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk5=/dev/disk/by-id/virtio-disk5 --passphrase-stdin --yes "
        f">/tmp/add-b 2>&1 & pid_b=$! ; "
        f"wait $pid_a ; echo $? > /tmp/exit-a ; "
        f"wait $pid_b ; echo $? > /tmp/exit-b"
    )

    # Assert each add succeeded independently.
    exit_a = int(machine.succeed("cat /tmp/exit-a").strip())
    exit_b = int(machine.succeed("cat /tmp/exit-b").strip())
    out_a = machine.succeed("cat /tmp/add-a")
    out_b = machine.succeed("cat /tmp/add-b")
    assert exit_a == 0, f"add-a failed (exit {exit_a}):\n{out_a}"
    assert exit_b == 0, f"add-b failed (exit {exit_b}):\n{out_b}"

    # pool.json must contain all 5 disks.
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    for d in ["disk1", "disk2", "disk3", "disk4", "disk5"]:
        assert d in pool["disks"], (
            f"{d} missing from pool.json: {set(pool['disks'].keys())}"
        )

    # btrfs must show 5 devices.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 5, (
        f"Expected 5 btrfs devices, got {devid_count}:\n{fi_show}"
    )

    # No residual journal.
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
