# Intent: `braid add` rejects an already-open add target mapper when the
# mapper is backed by a different kernel block device than the configured
# by-id target, even if both disks share a cloned LUKS UUID and label.
#
# Why it exists: the returned-disk add path used to rely on LUKS UUID at the
# mapper boundary. A cloned header can make the UUID match while the mapper is
# bound to the wrong disk.
#
# Scenario: disk3 is a removed-but-returnable braid disk from the mounted
# pool. Disk4 receives a cloned copy of disk3's LUKS header and is opened as
# braid-disk3. Braid must reject that foreign mapper before classifying btrfs
# FSID, leave state untouched, then succeed after the mapper is closed.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(name, yes=True):
    passphrase_q = shlex.quote(passphrase)
    yes_arg = " --yes" if yes else ""
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin{yes_arg}"
    )


def missing_devid():
    report = json.loads(machine.succeed("braid status --json"))
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}"
    return str(devids[0])


with subtest("Build pool with a returnable disk3"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'add cloned header data' > /mnt/storage/kept.txt")
    machine.succeed("sync")

    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    assert "missing" in machine.succeed("btrfs fi show /mnt/storage").lower()
    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")

    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk1" in member_names(pool), pool
    assert "disk2" in member_names(pool), pool
    assert "disk3" not in member_names(pool), pool
    assert "missing" not in machine.succeed("btrfs fi show /mnt/storage").lower()


with subtest("Clone disk3 header onto disk4 and open disk4 as braid-disk3"):
    passphrase_q = shlex.quote(passphrase)
    machine.succeed(
        "cryptsetup luksHeaderBackup "
        "--header-backup-file /tmp/disk3.hdr "
        "/dev/disk/by-id/virtio-disk3"
    )
    machine.succeed(
        "cryptsetup luksHeaderRestore --batch-mode "
        "--header-backup-file /tmp/disk3.hdr "
        "/dev/disk/by-id/virtio-disk4"
    )
    uuid3 = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk3").strip()
    uuid4 = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk4").strip()
    assert uuid3 == uuid4 and uuid3, f"expected cloned UUIDs, got {uuid3=} {uuid4=}"

    machine.succeed(
        f"printf '%s' {passphrase_q} | "
        "cryptsetup open --key-file=- /dev/disk/by-id/virtio-disk4 braid-disk3"
    )
    disk4_kernel = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk4").strip()
    status = machine.succeed("cryptsetup status braid-disk3")
    assert disk4_kernel in status, status

    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-add.json")


with subtest("Add refuses the cloned-header foreign mapper"):
    exit_code, output = machine.execute(f"{add_cmd('disk3')} 2>&1")
    assert exit_code != 0, f"add must refuse cloned-header mapper:\n{output}"
    disk3_kernel = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk3").strip()
    disk4_kernel = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk4").strip()
    for needle in [
        "is open but backed by",
        disk3_kernel,
        disk4_kernel,
        "Close the conflicting mapper",
    ]:
        assert needle in output, f"missing {needle!r} in:\n{output}"
    assert "contains no btrfs superblock" not in output, output

    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-add.json /var/lib/braid/pool.json")


with subtest("Closing the conflicting mapper lets returned-disk add proceed"):
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed(add_cmd("disk3"))
    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk3" in member_names(pool), pool
    assert machine.succeed("cat /mnt/storage/kept.txt").strip() == "add cloned header data"


machine.shutdown()
