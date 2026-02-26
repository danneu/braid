# Test: metadata profile is DUP after removing second-to-last disk
#
# Intent: When a 2-disk RAID1 pool is reduced to 1 disk via `braid remove`,
# the metadata profile must be DUP, not single.
#
# Why it exists: The original implementation used `-mconvert=single` when
# converting from RAID1 to single-device. This leaves metadata with zero
# redundancy — a single bad sector can lose the entire filesystem. DUP
# stores two copies of metadata on the same device, matching what mkfs.btrfs
# uses by default for single-device filesystems.
#
# Scenario: User has a 2-drive NAS and needs to remove a failing drive.
# After removal, the remaining single drive should have DUP metadata to
# maintain the same safety level as a freshly-formatted single-device btrfs.

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


# --- Setup: 2-disk RAID1 pool ---

with subtest("Build 2-disk RAID1 pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "Data, RAID1" in df_output, f"Expected RAID1 data:\n{df_output}"
    assert "Metadata, RAID1" in df_output, f"Expected RAID1 metadata:\n{df_output}"

    machine.succeed("echo 'test data' > /mnt/storage/file.txt")
    machine.succeed("sync")

# --- Remove disk2 (2 → 1 transition) ---

with subtest("Remove disk2 to go single-device"):
    machine.succeed("braid remove disk2 --yes")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 1, f"Expected 1 device, got {devid_count}:\n{fi_show}"

# --- The actual assertion: metadata must be DUP ---

with subtest("Metadata profile is DUP after 2-to-1 removal"):
    df_output = machine.succeed("btrfs fi df /mnt/storage")
    print(f"btrfs fi df output:\n{df_output}")

    assert "Data, single" in df_output, f"Expected single data profile:\n{df_output}"
    assert "Metadata, DUP" in df_output, f"Expected DUP metadata profile:\n{df_output}"

with subtest("Data intact after removal"):
    content = machine.succeed("cat /mnt/storage/file.txt").strip()
    assert content == "test data", f"Expected 'test data', got '{content}'"

machine.shutdown()
