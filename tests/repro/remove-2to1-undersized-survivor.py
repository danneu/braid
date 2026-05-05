# Repro: 2->1 remove with an undersized survivor.
#
# Intent:
# - `braid remove` must refuse at preflight when the sole remaining device
#   cannot hold all live data under the post-balance profile mix
#   (data -> single, metadata/system -> DUP).
#
# Why it exists:
# - Before the fix, `cmd_remove` skipped `check_eviction_space` for
#   `remaining == 1`, on the theory that the RAID1 -> single balance
#   handled redistribution. That is false for unequal-size pools:
#   RAID1 capacity is min(disk1, disk2) and fits, but post-balance demand
#   is data + 2*metadata + 2*system on one device. With disk2 (1 GiB) as
#   the survivor, metadata doubling tips the required bytes past the
#   survivor's physical capacity. `braid remove` writes pending-op.json
#   and then either ENOSPCs in `btrfs device remove` or crashes the fs
#   read-only mid-migration -- both force the operator into `braid
#   recover` mode.
#
# Scenario:
# - 2-disk RAID1 pool, 4 GiB + 1 GiB. Operator fills the pool from a
#   laptop (`dd`) until the survivor-capacity precondition is satisfied,
#   then runs `braid remove disk1` to shrink the pool. Post-fix, braid
#   refuses cleanly before any irreversible work. Pre-fix, braid commits
#   a journal and falls into the irreversible path.

import json
import re
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def survivor_usable_bytes() -> int:
    """Survivor (braid-disk2) device_size - device_slack, from `btrfs device usage --raw`."""
    raw = machine.succeed("btrfs device usage --raw /mnt/storage")
    # Output shape:
    #   /dev/mapper/braid-disk2, ID: 2
    #      Device size:           ...
    #      Device slack:          ...
    #      Data,RAID1:            ...
    #      ...
    #      Unallocated:           ...
    in_block = False
    size = None
    slack = None
    for line in raw.splitlines():
        if line.startswith("/dev/mapper/braid-disk2"):
            in_block = True
            continue
        if not line.startswith(" ") and not line.startswith("\t"):
            in_block = False
            continue
        if not in_block:
            continue
        m = re.match(r"\s+Device size:\s+(\d+)", line)
        if m:
            size = int(m.group(1))
            continue
        m = re.match(r"\s+Device slack:\s+(\d+)", line)
        if m:
            slack = int(m.group(1))
            continue
    if size is None or slack is None:
        raise AssertionError(f"could not parse survivor device size/slack:\n{raw}")
    return size - slack


def needed_post_single() -> int:
    """Bytes required on a lone survivor post-balance: data + 2*metadata + 2*system."""
    df = json.loads(machine.succeed("btrfs --format json filesystem df /mnt/storage"))
    data = sum(e["used"] for e in df["filesystem-df"] if e["bg-type"] == "Data")
    meta = sum(e["used"] for e in df["filesystem-df"] if e["bg-type"] == "Metadata")
    sysb = sum(e["used"] for e in df["filesystem-df"] if e["bg-type"] == "System")
    return data + 2 * meta + 2 * sysb


# --- Phase 1: Build 2-disk RAID1 pool, 4 GiB + 1 GiB ---

with subtest("Setup: build 2-disk pool with unequal sizes"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"

    initial_usage = machine.succeed("btrfs device usage --raw /mnt/storage")
    print(f"Initial device usage (before any writes):\n{initial_usage}")

# --- Phase 2: Fill pool with a dynamic threshold ---
#
# Shape Metadata.used aggressively. The 2->1 precondition is
# `Data + 2*Metadata + 2*System > survivor_usable` (~1008 MiB here).
# We skip bulk `dd` on purpose: a single data chunk allocation for
# bulk writes takes ~720 MiB of the 1008 MiB survivor, leaving only
# room for one 256 MiB metadata chunk -- so `Metadata.used` caps at
# ~256 MiB and the precondition never fires even with 480k+ files.
#
# Instead, grow Metadata.used alone. With no data chunks, btrfs can
# allocate ~3 metadata chunks of 256 MiB each on the survivor, letting
# `Metadata.used` reach ~750 MiB. The precondition then fires on
# `2 * Metadata.used` alone, well past the 1008 MiB survivor bound.
#
# Files are created with ~60 bytes of content -- below the default
# `max_inline` threshold (~2 KiB) -- so their content is stored
# directly in the inode item inside Metadata block groups, not in a
# Data extent. Each file adds ~60 B content + ~1 KiB inode + dir-item
# metadata; growth rate is ~11 MiB per 10,000 files.

with subtest("Fill pool until survivor capacity would be exceeded"):
    usable = survivor_usable_bytes()
    print(f"Survivor usable (device_size - device_slack) = {usable} bytes")

    def check_precondition(label: str) -> bool:
        needed = needed_post_single()
        print(f"{label}: needed_post_single={needed}, usable={usable}")
        return needed > usable

    satisfied = False

    # Mount with max_inline=4096 so content up to ~1.5 KiB reliably
    # inlines into Metadata nodes. Default max_inline is ~2048 which
    # typically drops to ~1000-1500 effective bytes after item-header
    # overhead, so some writes fall through to tiny Data extents and
    # keep the data chunk allocated.
    machine.succeed("mount -o remount,max_inline=4096 /mnt/storage")
    BATCHES = 80
    FILES_PER_BATCH = 10000
    # ~1500-byte content is below max_inline=4096 yet grows Metadata.used
    # by ~1.7 KiB/file (content + inode/dir item) -- ~3x faster than the
    # 60-byte shape, which is necessary because we need ~1 GiB of
    # Metadata.used to cross the 2 GiB survivor threshold.
    for b in range(BATCHES):
        machine.execute(f"mkdir -p /mnt/storage/m{b}")
        machine.execute(
            "python3 -c '"
            "import os;\n"
            f"d=\"/mnt/storage/m{b}\";\n"
            f"for i in range({FILES_PER_BATCH}):\n"
            "    open(f\"{d}/f{i}\", \"wb\").write(b\"A\" * 1500)'"
        )
        machine.execute("sync")
        if check_precondition(f"metadata batch {b} (~{(b + 1) * FILES_PER_BATCH} files)"):
            satisfied = True
            break

    if not satisfied:
        # Dump diagnostics so the failure output is self-describing.
        df_raw = machine.succeed("btrfs --format json filesystem df /mnt/storage")
        usage_raw = machine.succeed("btrfs device usage --raw /mnt/storage")
        raise AssertionError(
            f"setup error: could not drive pool into a state where "
            f"needed_post_single > usable ({usable}). Final df:\n{df_raw}\n"
            f"Final usage:\n{usage_raw}"
        )

    print(f"Precondition satisfied: needed_post_single={needed_post_single()} > usable={usable}")

# --- Phase 3: braid remove must refuse at preflight ---

with subtest("braid remove disk1 refuses before starting the balance"):
    (status, output) = machine.execute("braid remove disk1 --yes 2>&1")
    print(f"braid remove exit={status}, output:\n{output}")

    assert status != 0, f"expected non-zero exit, got 0:\n{output}"
    assert (
        "not enough space on surviving device" in output
    ), f"expected capacity-refusal message, got:\n{output}"

# --- Phase 4: no journal was written ---

with subtest("No pending-op.json after preflight refusal"):
    # machine.fail expects non-zero exit; `test -f` returns 1 when the
    # file does not exist. Pre-fix, the journal is written before the
    # irreversible `btrfs device remove` call, so this would succeed.
    machine.fail("test -f /var/lib/braid/pending-op.json")

# --- Phase 5: pool is still healthy ---

with subtest("Pool remains read-write and RAID1"):
    # Still writable.
    machine.succeed("touch /mnt/storage/post-refusal && rm /mnt/storage/post-refusal")
    # Still RAID1 (balance never started).
    df = json.loads(machine.succeed("btrfs --format json filesystem df /mnt/storage"))
    data_profiles = {e["bg-profile"] for e in df["filesystem-df"] if e["bg-type"] == "Data"}
    assert data_profiles == {"RAID1"}, f"expected RAID1 only, got {data_profiles}"

machine.shutdown()
