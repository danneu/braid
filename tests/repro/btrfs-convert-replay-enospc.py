# Repro: an interrupted RAID1 convert leaves mixed profiles, and a soft pass
# first lets the hard convert replay finish
#
# Intent: Lock the two btrfs properties `cli/src/pool.rs#pool_balance_raid1`
# rests on:
#   1. A hard `-dconvert=raid1 -mconvert=raid1` convert cancelled partway
#      through a 1->2 add leaves a chunk type holding more than one profile
#      (System DUP and RAID1 at once), and `btrfs --format json filesystem df`
#      -- the probe braid runs -- reports both.
#   2. On that pool, braid's soft convert then its hard convert both succeed,
#      and the pool ends with one RAID1 profile per chunk type.
#
# Why it exists: a hard convert run straight on that mixed pool failed with
# ENOSPC ("allocation failed flags 18", a SYSTEM|RAID1 tree block) in 15 of
# 16 trials, so `braid recover` failed closed on every interrupted 1->2 add
# and the operator had to run it twice. braid now probes the profiles and runs
# the soft pass first. If a btrfs-progs or kernel pin bump changed how a
# cancelled convert leaves its chunks, or broke the soft-then-hard sequence,
# the probe would stop seeing the state it keys on. This lock does not assert
# that the hard convert alone fails: it fails most of the time, not every
# time. repro-* checks are the manual pin-bump gate
# (`docs/dev/testing.md#live-tool-behavior-locks`).
#
# Scenario: a UPS low-battery shutdown interrupts the post-add RAID1 convert
# of a 1->2 `braid add` (`tests/module/ups-lb-during-balanced-add.py`), and
# `braid recover` replays it.

start_all()
machine.wait_for_unit("multi-user.target")
dm_delay_create(machine, "disk2", by_id_symlink=False)

DISK1 = "/dev/disk/by-id/virtio-disk1"
DISK2 = "/dev/mapper/disk2-delay"
MOUNT = "/mnt/pool"
# braid's argv: cli/src/cmd.rs#CmdRequest::to_argv.
HARD = "btrfs balance start --enqueue -dconvert=raid1 -mconvert=raid1 " + MOUNT
SOFT = "btrfs balance start --enqueue -dconvert=raid1,soft -mconvert=raid1,soft " + MOUNT
TRIALS = 3


def profiles():
    """Chunk type -> set of profiles, from the df probe braid runs."""
    df = json.loads(machine.succeed("btrfs --format json filesystem df " + MOUNT))
    seen = {}
    for row in df["filesystem-df"]:
        if row["bg-type"] != "GlobalReserve":
            seen.setdefault(row["bg-type"], set()).add(row["bg-profile"])
    return seen


def one_disk_pool_gains_a_disk():
    machine.execute("umount " + MOUNT)
    machine.succeed(f"wipefs -a {DISK1} {DISK2}")
    machine.succeed("mkfs.btrfs -f " + DISK1)
    machine.succeed(f"mkdir -p {MOUNT} && mount {DISK1} {MOUNT}")
    machine.succeed(f"dd if=/dev/urandom of={MOUNT}/payload bs=1M count=100 status=none && sync")
    machine.succeed(f"btrfs device add -f {DISK2} {MOUNT}")


def cancel_hard_convert_midway():
    # Slow writes to the new disk keep the convert running until the data
    # chunks start, after the system chunk has converted.
    dm_delay_activate(machine, "disk2", write_delay_ms=500)
    machine.succeed("dmesg -C")
    machine.succeed(f"btrfs balance start --bg -dconvert=raid1 -mconvert=raid1 {MOUNT}")
    machine.wait_until_succeeds(
        "dmesg | grep -q 'relocating block group .* flags data$'", timeout=120
    )
    machine.execute("btrfs balance cancel " + MOUNT)
    machine.wait_until_succeeds(
        f"btrfs balance status {MOUNT} | grep -q 'No balance'", timeout=120
    )
    dm_delay_deactivate(machine, "disk2")
    # A remount, as recover sees the pool after the shutdown.
    machine.succeed("umount " + MOUNT)
    machine.succeed(f"mount {DISK1} {MOUNT}")


for trial in range(TRIALS):
    with subtest(f"trial {trial}: a cancelled convert leaves mixed profiles"):
        one_disk_pool_gains_a_disk()
        cancel_hard_convert_midway()
        mixed = profiles()
        print(f"profiles after cancel: {mixed}")
        assert len(mixed.get("System", set())) > 1, (
            "a cancelled hard convert no longer leaves System in two profiles; "
            f"the probe braid keys on would miss it: {mixed}"
        )

    with subtest(f"trial {trial}: soft then hard convert finishes the replay"):
        machine.succeed(SOFT)
        machine.succeed(HARD)
        final = profiles()
        print(f"profiles after replay: {final}")
        assert final == {t: {"RAID1"} for t in ("Data", "Metadata", "System")}, (
            f"replay did not leave one RAID1 profile per chunk type: {final}"
        )
