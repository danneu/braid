# Test: braid doctor detects mixed metadata profiles
#
# Intent: Verify metadata_profile_mismatch warns when metadata block
# groups have different profiles (e.g. RAID1 + single).
#
# Why: Mixed metadata is more dangerous than mixed data — metadata loss
# can make the entire filesystem unrecoverable. An interrupted
# `btrfs balance` can leave metadata in this state.
#
# Scenario: Build a 2-disk RAID1 pool, fill the 256 MiB metadata block
# group with inline files to force a second BG allocation, then convert
# one BG to single with limit=1. braid doctor should detect and warn.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key} --passphrase-stdin --yes"
    )

# Intent: Build the RAID1 pool that subsequent subtests operate on.
# Why: All subtests need a mounted 2-disk RAID1 btrfs filesystem.
# Scenario: Fresh NAS setup — two disks added with braid add.
with subtest("Build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

# Intent: Force btrfs to allocate a second metadata block group.
# Why: btrfs metadata BGs are 256 MiB. With normal usage, the initial
#   BG has ample free space and btrfs won't allocate a second one.
#   Inline files (< 2048 bytes) are stored directly in the metadata
#   B-tree, so creating many of them fills the metadata BG efficiently.
# Scenario: We need 2+ metadata BGs so that limit=1 can convert
#   exactly one, leaving the other with the original RAID1 profile.
with subtest("Fill metadata block group with inline files"):
    machine.succeed("mkdir -p /mnt/storage/fill")
    # Create 120K files of 2000 bytes each. Files under max_inline
    # (default 2048) are stored inline in the metadata B-tree.
    # 120K * ~2200 bytes effective = ~264 MiB, exceeding the 256 MiB BG.
    machine.succeed(
        "dd if=/dev/zero bs=2000 count=120000 status=none | "
        "split -b 2000 -d -a 6 - /mnt/storage/fill/"
    )
    machine.succeed("sync")

# Intent: Verify that filling metadata actually triggered a second BG.
# Why: If only 1 metadata BG exists, limit=1 converts everything and
#   the test passes or fails for the wrong reason.
# Scenario: Guard against btrfs allocator changes that might handle
#   inline files differently (e.g. falling back to data extents).
with subtest("Precondition: second metadata BG was allocated"):
    df_raw = machine.succeed(
        "btrfs --format json filesystem df /mnt/storage"
    )
    print(f"Pre-conversion df:\n{df_raw}")
    df = json.loads(df_raw)
    meta_total = sum(
        e["total"]
        for e in df["filesystem-df"]
        if e["bg-type"] == "Metadata"
    )
    # One metadata BG is 268435456 bytes (256 MiB). If total exceeds
    # that, btrfs allocated at least one additional BG.
    assert meta_total > 268435456, (
        f"Expected metadata total > 256 MiB (indicating 2+ BGs), "
        f"got {meta_total} bytes. "
        f"Full df: {df_raw}"
    )

# Intent: Create mixed metadata profiles by converting one BG to single.
# Why: This is the exact state an interrupted btrfs balance leaves behind.
# Scenario: Operator starts `btrfs balance -mconvert=raid1` after adding
#   a disk, but it's interrupted (power loss, cancel). Some metadata BGs
#   remain single while others are already RAID1.
with subtest("Convert one metadata BG to single"):
    machine.succeed(
        "btrfs balance start --force -mconvert=single,limit=1 /mnt/storage"
    )
    df_raw = machine.succeed(
        "btrfs --format json filesystem df /mnt/storage"
    )
    print(f"Post-conversion df:\n{df_raw}")
    df = json.loads(df_raw)
    meta_profiles = {
        e["bg-profile"]
        for e in df["filesystem-df"]
        if e["bg-type"] == "Metadata"
    }
    assert len(meta_profiles) > 1, (
        f"Expected mixed metadata profiles, got {meta_profiles}. "
        f"Full df: {df_raw}"
    )

# Intent: Verify braid doctor detects mixed metadata and warns.
# Why: Mixed metadata is more dangerous than mixed data — losing
#   metadata can make the entire filesystem unrecoverable.
# Scenario: Operator runs `braid doctor` after an interrupted balance
#   and sees a clear warning with a remediation command.
with subtest("Metadata profile mismatch — mixed metadata warns"):
    raw = machine.succeed("braid doctor --json")
    print(f"Doctor JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["metadata_profile_mismatch"]["status"] == "warn", (
        f"metadata_profile_mismatch: {checks['metadata_profile_mismatch']}"
    )
    assert "mixed" in checks["metadata_profile_mismatch"]["message"], (
        f"Expected 'mixed' in message: "
        f"{checks['metadata_profile_mismatch']['message']}"
    )

machine.shutdown()
