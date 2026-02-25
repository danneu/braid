# Test: config disk-key immutability
#
# What: Builds a pool and disk-map entries, then renames one disk key in config
# while keeping the same by-id path and runs a mutating command.
#
# Why: v1.0 forbids key rename/reassignment in mutating commands; they must
# fail fast before probing or making storage changes.
#
# Dependencies: braid add succeeds and writes disk-map entries.

import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    return (
        f"BRAID_PASSPHRASE='{passphrase}' "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --yes"
    )


with subtest("Setup: build 2-disk pool and disk-map entries"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show

    raw_map = machine.succeed("cat /var/lib/braid/disk-map.json")
    disk_map = json.loads(raw_map)
    assert "disk1" in disk_map["disks"], disk_map
    assert "disk2" in disk_map["disks"], disk_map

with subtest("Rename key in config and run mutating command"):
    machine.succeed(
        """cat > /tmp/renamed-config.json <<'JSON'
{
  "disks": {
    "wd-red": { "by_id": "/dev/disk/by-id/virtio-disk1" },
    "disk2": { "by_id": "/dev/disk/by-id/virtio-disk2" }
  },
  "mount_point": "/mnt/storage"
}
JSON"""
    )

    map_before = machine.succeed("cat /var/lib/braid/disk-map.json")
    status, output = machine.execute(
        "braid --config /tmp/renamed-config.json remove-missing --yes 2>&1"
    )
    assert status != 0, f"expected non-zero exit, got {status}:\n{output}"

    expected = (
        "Disk key rename/reassignment is not allowed in v1.0. "
        "Keep original key 'disk1' or use explicit replace/remove+add workflow. "
        "Details: recorded key 'disk1' with by_id '/dev/disk/by-id/virtio-disk1' "
        "now appears as 'wd-red'."
    )
    assert expected in output, f"expected exact immutability error:\n{output}"

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "/dev/mapper/braid-disk1" in fi_show, fi_show
    assert "/dev/mapper/braid-disk2" in fi_show, fi_show
    assert "missing" not in fi_show.lower(), fi_show

    map_after = machine.succeed("cat /var/lib/braid/disk-map.json")
    assert map_after == map_before, "disk-map changed on rejected key rename"

machine.shutdown()
