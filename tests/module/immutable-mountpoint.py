# Test: immutable-mountpoint
#
# Intent: Verify braid seals the offline pool mountpoint immutable so a write
# while the pool is unmounted fails with EPERM instead of silently landing on
# the root filesystem and being shadowed when the pool mounts. Exercises the
# boot seal, mount-over-immutable, persistence across lock/unlock, the
# mounted-root safety guard, the STATX_ATTR_MOUNT_ROOT bind-mount predicate, the
# doctor detection signal, the explicit seal/unseal levers, the activation
# self-heal, and the seal-before-mount ordering on an auto-unlock system.
#
# Why it exists: the unmounted-mountpoint write-to-root bug is a data-safety
# hazard. A dropped boot unit, a lost before-auto-unlock edge, a sealed live
# pool root, or a seal that does not survive a lock cycle each silently reopens
# it. The VM is the only place the real kernel ioctl round-trip and the systemd
# ordering are exercised together.
# See docs/design/decisions/028-immutable-unmounted-mountpoint.md.
#
# Scenario: a 2-disk RAID1 pool pre-created by the initrd fixture. The
# manual-unlock node (`machine`) boots offline so the boot seal runs in the
# pre-mount window (cases 1-8 and 10). A separate auto-unlock node
# (`autoMachine`) comes online at boot via braid-auto-unlock.service -- a system
# that never boots offline -- to prove the seal still runs before the mount
# (case 9). The root filesystem backing /mnt is the VM's ext4 root, which
# supports FS_IMMUTABLE_FL; an unsupported root would degrade to a warning
# instead (covered by Rust unit tests, not here).

import json

passphrase = "testpassphrase"
MP = "/mnt/storage"


def lsattr_flags(node, path):
    # lsattr -d emits "----i---------------- /path"; return the flags token.
    out = node.succeed(f"lsattr -d {path}").strip()
    return out.split()[0]


def assert_immutable(node, path):
    flags = lsattr_flags(node, path)
    assert "i" in flags, f"{path} expected immutable, lsattr flags={flags!r}"


def assert_mutable(node, path):
    flags = lsattr_flags(node, path)
    assert "i" not in flags, f"{path} expected mutable, lsattr flags={flags!r}"


def doctor_check_status(node, name):
    # Use execute, not succeed: braid doctor exits non-zero if any check fails,
    # but stdout still carries the JSON report we want to inspect.
    _status, out = node.execute("braid doctor --json")
    report = json.loads(out)
    for c in report["checks"]:
        if c["name"] == name:
            return c["status"]
    raise AssertionError(f"check {name} not found in doctor report: {out}")


def show(node, unit, prop):
    return node.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def unlock(node):
    node.succeed(f"printf '%s\\n' {passphrase} | braid unlock --passphrase-stdin")
    node.succeed(f"mountpoint -q {MP}")


def lock(node):
    node.succeed("braid lock")
    node.fail(f"mountpoint -q {MP}")


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)
autoMachine.wait_for_unit("multi-user.target", timeout=180)

# --- Case 1: Offline immutable after boot ---

with subtest("boot seal makes the offline mountpoint immutable"):
    # The seal comes from braid-seal-mountpoint.service at boot, NOT from unlock.
    machine.fail(f"mountpoint -q {MP}")
    assert_immutable(machine, MP)
    # An offline write is rejected with EPERM...
    machine.fail(f"touch {MP}/x")
    # ...and the kernel refuses rmdir of an immutable dir, so a sealed offline
    # mountpoint cannot be silently removed and recreated mutable.
    machine.fail(f"rmdir {MP}")
    # Positive half of the doctor wiring: the check is registered, probes the
    # configured path, and stays quiet (ok, not warn) when the invariant holds.
    assert doctor_check_status(machine, "mountpoint_immutable") == "ok"

with subtest("seal unit carries the root sandbox"):
    unit = machine.succeed("systemctl cat braid-seal-mountpoint.service")
    assert "ProtectSystem=strict" in unit, (
        "seal unit must use ProtectSystem=strict:\n" + unit
    )
    assert "ReadWritePaths=/mnt" in unit, (
        "seal unit must keep the mountpoint parent writable without making "
        "the guarded path a private mount root:\n" + unit
    )
    assert "CapabilityBoundingSet=CAP_LINUX_IMMUTABLE" in unit, (
        "seal unit must keep only immutable-flag capability:\n" + unit
    )
    assert show(machine, "braid-seal-mountpoint.service", "PrivateNetwork") == "yes"
    assert show(machine, "braid-seal-mountpoint.service", "PrivateDevices") == "yes"
    assert show(machine, "braid-seal-mountpoint.service", "NoNewPrivileges") == "yes"

# --- Case 6: tmpfiles issues no chmod/chown against the sealed dir ---

with subtest("systemd-tmpfiles tolerates the sealed mountpoint"):
    # The tmpfiles rule `d /mnt/storage 0755 root root` matches the on-disk
    # mode/owner, so tmpfiles attempts neither chmod nor chown -> no EPERM
    # against +i. Re-running tmpfiles against the already-sealed dir is the only
    # way to exercise this (first boot seals AFTER tmpfiles-setup runs).
    machine.succeed("systemd-tmpfiles --create")
    assert_immutable(machine, MP)
    machine.fail(f"touch {MP}/x")

# --- Case 7: STATX_ATTR_MOUNT_ROOT refuses a same-device bind mount ---

with subtest("bind mount over the mountpoint is not sealed"):
    machine.succeed("mkdir -p /tmp/scratch")
    machine.succeed(f"mount --bind /tmp/scratch {MP}")
    # The bind-mount root is mutable; seal-mountpoint must skip it (SkippedMounted)
    # rather than seal a mount root an st_dev-only check would miss.
    machine.succeed("braid seal-mountpoint")
    assert_mutable(machine, MP)
    machine.succeed(f"umount {MP}")
    # The underlying bare dir kept its boot seal.
    assert_immutable(machine, MP)

# --- Case 2 + 5: mount-over-immutable works; the live root is never sealed ---

with subtest("unlock mounts over the sealed dir and writes land on the pool"):
    unlock(machine)
    # Writing into the MOUNTED pool succeeds (mount-over-immutable).
    machine.succeed(f"touch {MP}/canary")
    assert machine.succeed(f"findmnt -n -o FSTYPE {MP}").strip() == "btrfs"
    # Safety: the mounted pool root is NOT immutable -- braid never sealed a live root.
    assert_mutable(machine, MP)
    machine.succeed(f"touch {MP}/x")

# --- Case 3: round-trip -- seal persists across a lock/unlock cycle ---

with subtest("seal persists across lock and re-unlock"):
    lock(machine)
    # Same path rejects writes again: the boot seal survived the cycle.
    assert_immutable(machine, MP)
    machine.fail(f"touch {MP}/x")
    unlock(machine)
    machine.succeed(f"touch {MP}/again")
    assert machine.succeed(f"findmnt -n -o FSTYPE {MP}").strip() == "btrfs"

# --- Case 4: idempotency ---

with subtest("repeated lock/unlock keeps the offline dir sealed"):
    lock(machine)
    unlock(machine)
    lock(machine)
    assert_immutable(machine, MP)
    machine.fail(f"touch {MP}/x")

# --- Case 8: out-of-band unseal detection + the --unseal lever contract ---

with subtest("doctor warns on an out-of-band unseal and refuses the configured path"):
    # chattr -i is the out-of-band path the doctor Warn exists to catch (the
    # appliance wrapper has no chattr). Using the lever here is impossible by
    # design: --unseal refuses the configured path.
    machine.succeed(f"chattr -i {MP}")
    assert_mutable(machine, MP)
    assert doctor_check_status(machine, "mountpoint_immutable") == "warn"
    # F4: --unseal refuses the currently configured mount point (exit non-zero).
    machine.fail(f"braid seal-mountpoint --unseal {MP}")
    # A cleared offline dir is removable.
    machine.succeed(f"rmdir {MP}")

with subtest("the explicit seal/unseal levers protect and clear a non-configured path"):
    machine.succeed("mkdir /mnt/orphan")
    machine.succeed("braid seal-mountpoint /mnt/orphan")
    assert_immutable(machine, "/mnt/orphan")
    # Clearing an orphan succeeds...
    machine.succeed("braid seal-mountpoint --unseal /mnt/orphan")
    assert_mutable(machine, "/mnt/orphan")
    # ...and a repeat unseal of an already-mutable path is success, not failure (F2).
    machine.succeed("braid seal-mountpoint --unseal /mnt/orphan")
    # --unseal against a live mount root is refused (SkippedMounted -> non-zero).
    machine.succeed("mkdir -p /mnt/mp /tmp/src")
    machine.succeed("mount --bind /tmp/src /mnt/mp")
    machine.fail("braid seal-mountpoint --unseal /mnt/mp")
    machine.succeed("umount /mnt/mp")

# --- Case 10: activation self-heal + mounted-safety ---

with subtest("the seal oneshot re-runs and re-seals an out-of-band-unsealed offline dir"):
    # Restore the mountpoint case 8 removed, left mutable.
    machine.succeed(f"mkdir -p {MP}")
    machine.succeed(f"chattr -i {MP}")
    assert_mutable(machine, MP)
    # Self-heal mechanism: the seal unit is WantedBy=multi-user.target, so a
    # `nixos-rebuild switch` re-enqueues it, and as a dead Type=oneshot it re-runs
    # on (re)start. Assert the wiring, then exercise the re-run directly --
    # switch-to-configuration is not in the test VM's closure (this nixpkgs builds
    # a separate activation, not a toplevel switch binary), and the unit re-run is
    # the braid-owned behavior that matters.
    machine.succeed(
        "systemctl show -p WantedBy braid-seal-mountpoint.service | grep -q multi-user.target"
    )
    machine.succeed("systemctl start braid-seal-mountpoint.service")
    assert_immutable(machine, MP)
    machine.fail(f"touch {MP}/x")

with subtest("the seal oneshot is condition-gated off while the pool is mounted"):
    unlock(machine)
    machine.succeed(f"touch {MP}/canary2")
    # ConditionPathIsMountPoint=! makes systemd skip the unit while mounted (a
    # condition-skipped start still exits 0), so the live pool root is never
    # sealed -- the mounted-activation safety property, tested at the unit's gate.
    machine.succeed("systemctl start braid-seal-mountpoint.service")
    assert_mutable(machine, MP)
    machine.succeed(f"touch {MP}/x2")
    lock(machine)

# --- Case 9: seal-before-mount ordering on the auto-unlock node ---

with subtest("seal is ordered before braid-auto-unlock (deterministic edge)"):
    # After= is the inverse of the seal unit's Before=, so systemd materializes
    # the reverse edge regardless of the boot race. This fails IFF a refactor
    # drops the ordering edge.
    autoMachine.succeed(
        "systemctl show -p After braid-auto-unlock.service | grep -q braid-seal-mountpoint"
    )

with subtest("auto-unlock boots online over a dir that was sealed pre-mount"):
    # End-to-end sanity (not a standalone guard -- the seal almost always wins the
    # race even without the edge; the deterministic check above is what catches a
    # dropped edge).
    autoMachine.succeed(f"mountpoint -q {MP}")
    autoMachine.succeed("braid lock")
    autoMachine.fail(f"mountpoint -q {MP}")
    assert_immutable(autoMachine, MP)
    autoMachine.fail(f"touch {MP}/x")
