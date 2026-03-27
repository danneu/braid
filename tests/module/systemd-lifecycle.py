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
# plus a spare disk3 for the add test. Tests exercise:
# (1) systemctl start braid-pool.target brings pool online and activates
#     braid-online.service,
# (2) systemctl stop braid-online.service unmounts pool and closes LUKS,
# (3) braid unlock/lock via CLI wrapper correctly activates/deactivates
#     braid-online.service,
# (4) braid add via CLI wrapper activates braid-online.service,
# (5) wrapper prints warning but still succeeds when braid-online.service
#     cannot be activated.

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

# --- Subtest 2: braid-pool.target round-trip ---

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

    # Reset braid-unlock.service state. RemainAfterExit=true keeps it
    # "active (exited)" after subtest 2, which would prevent re-use of the
    # systemctl start braid-pool.target path. Not strictly needed for
    # subtests 4-7 (use CLI directly), but prevents fragility if subtests
    # are reordered later.
    machine.succeed("systemctl stop braid-unlock.service")

# --- Subtest 4: CLI wrapper synchronization (unlock) ---

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

# --- Subtest 6: braid add activates braid-online.service ---

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

    # Verify the override makes the service fail.
    machine.fail("systemctl start braid-online.service")

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

machine.shutdown()
