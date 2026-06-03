# Intent:
# - `braid add` into a degraded pool (a member missing) joins the new disk but
#   SKIPS the RAID1 convert balance: the pool stays degraded, the new disk stays
#   empty, and a single `[skip] pool: RAID1 balance skipped ...` note is printed.
#   The follow-up `braid remove-missing --missing-id <devid>` is what restores
#   redundancy -- ending with a healthy RAID1 pool across the two present disks.
#
# Why it exists:
# - The degraded add deliberately defers redundancy restoration to the
#   purpose-built repair path instead of running the hard RAID1 convert (which
#   would rewrite every chunk while the pool has no redundancy). This pins the
#   *safety rationale* for the skip end-to-end: skipping at add is safe BECAUSE
#   the documented `add`-then-`remove-missing` workflow restores RAID1 at the
#   repair step. Unit tests pin the skip gate; this proves the full recipe.
#
# Scenario:
# - A 2-disk RAID1 NAS loses disk2. `remove-missing` alone would refuse (can't
#   drop RAID1 below two devices), so the operator first `braid add disk3`
#   (pool stays degraded, disk3 empty), then `braid remove-missing` to drop the
#   dead member and rebalance onto disk3, converging to a healthy pool.

import json
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def read_pool():
    return json.loads(machine.succeed("cat /var/lib/braid/pool.json"))


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def missing_devid():
    report = json.loads(machine.succeed("braid status --json"))
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}"
    return str(devids[0])


def device_used(mapper_name):
    """The `used` field for a mapper from `btrfs fi show` (e.g. '0.00B')."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"used\s+(\S+)", line)
            if m:
                return m.group(1)
    raise AssertionError(f"used not found for {mapper_name} in:\n{fi_show}")


# --- Phase 0: build a 2-disk RAID1 pool with data ---

with subtest("Setup: build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df, f"expected RAID1, got:\n{df}"
    machine.succeed("echo 'important data' > /mnt/storage/precious.txt")
    machine.succeed("sync")

# --- Phase 1: synthesize a missing device (disk2) ---

with subtest("Kill disk2: unmount, close mapper, remount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), f"expected missing device:\n{fi_show}"

# --- Phase 2: degraded add of disk3 -> joins but skips the balance ---

with subtest("braid add disk3 joins the disk but skips the RAID1 balance"):
    machine.succeed(f"{add_cmd('disk3')} >/tmp/add.out 2>/tmp/add.err")
    add_err = machine.succeed("cat /tmp/add.err")

    skip_line = (
        "[skip] pool: RAID1 balance skipped -- pool still has a missing"
        " device; redundancy not restored. Run `braid remove-missing` or"
        " `braid replace` to restore it."
    )
    assert add_err.count(skip_line) == 1, (
        "degraded add must surface the balance-skip note exactly once;"
        " stderr={!r}".format(add_err)
    )
    # No hard-convert balance ran: neither progress line appears.
    assert "balancing to RAID1" not in add_err, (
        "degraded add must NOT run the RAID1 balance ([wait]); stderr={!r}".format(add_err)
    )
    assert "RAID1 balance complete" not in add_err, (
        "degraded add must NOT run the RAID1 balance ([ok]); stderr={!r}".format(add_err)
    )

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk3" in fi_show, f"disk3 must join the pool:\n{fi_show}"
    assert "missing" in fi_show.lower(), f"pool must stay degraded after add:\n{fi_show}"
    assert fi_show.count("devid") == 3, f"expected 3 devids after add:\n{fi_show}"

    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df, f"RAID1 profile must survive the degraded add:\n{df}"

    # The skip means no data was relocated: disk3 is present but empty.
    used = device_used("braid-disk3")
    assert used.startswith("0.00"), (
        f"disk3 must be empty after a skipped balance, got used={used!r}"
    )
    machine.fail("test -e /var/lib/braid/pending-op.json")

# --- Phase 3: remove-missing restores redundancy across disk1 + disk3 ---

with subtest("braid remove-missing drops disk2 and restores RAID1"):
    # Diagnostic: the survivor-capacity preflight bottlenecks on the disk
    # that still holds a full RAID1 copy (disk1), so the fixture disks must
    # be large enough that disk1's unallocated headroom exceeds the missing
    # device's allocated data. Print the geometry to make any sizing
    # regression self-explanatory in the VM log.
    print(machine.succeed("btrfs device usage /mnt/storage"))
    print(machine.succeed("btrfs fi df /mnt/storage"))

    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), f"pool must be healthy after remove-missing:\n{fi_show}"
    assert "braid-disk2" not in fi_show, f"dead disk2 must be gone:\n{fi_show}"
    assert "/dev/mapper/braid-disk1" in fi_show, f"disk1 must remain:\n{fi_show}"
    assert "/dev/mapper/braid-disk3" in fi_show, f"disk3 must remain:\n{fi_show}"
    assert fi_show.count("devid") == 2, f"expected 2 devids after remove-missing:\n{fi_show}"

    df = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df, f"RAID1 must be restored across the two present disks:\n{df}"

with subtest("Data survives the add-then-remove-missing repair"):
    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"unexpected data: {content!r}"

with subtest("Pool membership reflects disk2 dropped, disk1 + disk3 present"):
    pm = read_pool()
    assert member_names(pm) == {"disk1", "disk3"}, f"unexpected membership: {pm}"
    machine.fail("test -e /var/lib/braid/pending-op.json")

machine.shutdown()
