# Test: braid remove-missing collapses a 2-disk RAID1 pool with one disk missing
#
# Intent:
# - What behavior this test verifies.
#   - `braid remove-missing --missing-id <devid>` against a 2-disk RAID1
#     pool with one disk missing converts the pool to single data and DUP
#     metadata on the present disk, then removes the missing devid. The
#     pool ends with one device, no missing devid, `Data, single`,
#     `Metadata, DUP` and `System, DUP`, and the data intact.
#   - `--dry-run` shows the convert step before the device remove and
#     changes nothing.
#   - The prompt warns that the pool will have 1 disk and no redundancy.
#   - pool.json keeps only the present disk, and no pending-op.json is
#     left behind.
#
# Why it exists:
# - What risk/regression this protects against.
#   - The kernel's `btrfs_rm_device` refuses to drop below the RAID1
#     minimum of two devices, so the convert must run first. The collapse
#     relies on two kernel facts no unit test can prove: balance allows
#     single and DUP on one writable device, and once the last RAID1
#     block group is gone, the device remove passes the RAID1 minimum
#     check. If either breaks, the operator with no spare disk has no
#     route off a disk that is failing
#     (docs/design/decisions/012-intent-cli.md#two-to-one-remove).
#
# Scenario:
# - 2-disk NAS, disk2 dies (or the operator disconnects a disk that a
#   2->1 `braid remove` refused for its errors). The operator has no spare
#   and runs `braid remove-missing`, which leaves a working 1-disk pool.
#
# Missing-disk setup reuses the canonical pattern from
# tests/cli/remove-missing-inhibits-suspend.py: umount -> cryptsetup
# close -> mount -o degraded.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
pq = shlex.quote(passphrase)


def add_cmd(key):
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def get_missing_devid():
    """Get the devid of the missing device from braid status --json."""
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, "expected one missing devid in braid status:\n" + raw
    return str(devids[0])


# --- Phase 1: Build a 2-disk pool with data on it ---

with subtest("Setup: build 2-disk RAID1 pool with data"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("mountpoint -q /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2"]:
        assert "/dev/mapper/" + name in fi_show, name + " missing:\n" + fi_show

    machine.succeed("head -c 64M /dev/urandom > /mnt/storage/blob")
    machine.succeed("sync")
    blob_sum = machine.succeed("sha256sum /mnt/storage/blob").split()[0]

# --- Phase 2: Simulate disk2 death and mount degraded ---

with subtest("Simulate disk2 death and mount degraded"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" in fi_show.lower(), (
        "Expected 'missing' in btrfs filesystem show:\n" + fi_show
    )
    missing_devid = get_missing_devid()
    print(f"missing devid: {missing_devid}")

# --- Phase 3: Dry-run shows the convert before the remove ---

convert_step = "btrfs balance (RAID1 -> single data, DUP metadata)"
remove_step = f"btrfs device remove {missing_devid}"

with subtest("braid remove-missing --dry-run shows the convert first"):
    output = machine.succeed(
        f"braid remove-missing --missing-id {missing_devid} --dry-run 2>&1"
    )
    print(f"remove-missing --dry-run:\n{output}")
    assert convert_step in output, output
    assert remove_step in output, output
    assert output.find(convert_step) < output.find(remove_step), output
    machine.fail("test -f /var/lib/braid/pending-op.json")
    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    assert "Data, RAID1" in fi_df, "dry-run must not convert:\n" + fi_df

# --- Phase 4: Real run, accepted at the prompt ---

with subtest("braid remove-missing collapses the pool onto disk1"):
    status, output = machine.execute(
        f"printf 'yes\\n' | braid remove-missing --missing-id {missing_devid} 2>&1"
    )
    print(f"remove-missing (exit {status}):\n{output}")
    assert status == 0, f"collapse must succeed, got exit {status}:\n{output}"
    assert "Pool: 1 present + 1 missing -> 1 disk" in output, output
    assert "WARNING: Pool will have 1 disk -- no RAID1 redundancy." in output, output
    bal_wait = "[wait] pool: balancing RAID1 to single data, DUP metadata..."
    bal_ok = "[ok]   pool: balanced to single data, DUP metadata"
    rm_wait = f"[wait] pool: removing missing devid {missing_devid}..."
    rm_ok = f"[ok]   pool: missing devid {missing_devid} removed"
    for row in [bal_wait, bal_ok, rm_wait, rm_ok]:
        assert row in output, f"missing row {row!r}:\n{output}"
    assert (
        output.find(bal_wait)
        < output.find(bal_ok)
        < output.find(rm_wait)
        < output.find(rm_ok)
    ), output

# --- Phase 5: The pool is one disk, single + DUP, data intact ---

with subtest("Pool is one disk with single data and DUP metadata"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "missing" not in fi_show.lower(), fi_show
    assert "Total devices 1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show

    fi_df = machine.succeed("btrfs filesystem df /mnt/storage")
    for row in ["Data, single", "Metadata, DUP", "System, DUP"]:
        assert row in fi_df, f"expected {row!r}:\n{fi_df}"
    assert "RAID1" not in fi_df, "no RAID1 chunk may remain:\n" + fi_df

    after = machine.succeed("sha256sum /mnt/storage/blob").split()[0]
    assert after == blob_sum, f"data changed: {blob_sum} -> {after}"

with subtest("pool.json keeps only disk1 and no journal is left"):
    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    names = {member["name"] for member in pool["disks"].values()}
    assert names == {"disk1"}, json.dumps(pool, indent=2)
    machine.fail("test -f /var/lib/braid/pending-op.json")

    report = json.loads(machine.succeed("braid status --json"))
    assert report.get("missing_devids", []) == [], json.dumps(report, indent=2)

with subtest("The disconnected disk keeps its LUKS header"):
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk2")

machine.shutdown()
