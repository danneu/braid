# Intent: `braid replace` rejects an already-open new-target mapper when the
# mapper is backed by a different kernel block device than the configured
# by-id target, even if the backing device has a cloned LUKS UUID.
#
# Why it exists: cloned LUKS headers make UUID equality insufficient at the
# live mapper boundary; without a backing-path check, `btrfs replace start`
# could write pool data to the foreign disk.
#
# Scenario: an operator prepares disk3 as the replacement target, but a
# manually-opened braid-disk3 mapper is actually bound to disk4 whose LUKS
# header was cloned from disk3. Braid must fail before journal write or btrfs
# replace, then succeed after the conflicting mapper is closed.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"{name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd():
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        "braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        "--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        "--old disk2 --new disk3=/dev/disk/by-id/virtio-disk3 "
        "--passphrase-stdin --yes"
    )


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


with subtest("Build healthy 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("echo 'replace cloned header data' > /mnt/storage/kept.txt")
    machine.succeed("sync")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for mapper in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{mapper}" in fi_show, fi_show


with subtest("Prepare disk3 and clone its LUKS header onto disk4"):
    passphrase_q = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {passphrase_q} | "
        "cryptsetup luksFormat --batch-mode --key-file=- "
        "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
        "/dev/disk/by-id/virtio-disk3"
    )
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

    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-replace.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-before-replace.txt")


with subtest("Replace refuses the cloned-header foreign mapper"):
    exit_code, output = machine.execute(f"{replace_cmd()} 2>&1")
    assert exit_code != 0, f"replace must refuse cloned-header mapper:\n{output}"
    disk3_kernel = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk3").strip()
    disk4_kernel = machine.succeed("readlink -f /dev/disk/by-id/virtio-disk4").strip()
    for needle in [
        "is open but backed by",
        disk3_kernel,
        disk4_kernel,
        "Close the conflicting mapper",
    ]:
        assert needle in output, f"missing {needle!r} in:\n{output}"

    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-replace.json /var/lib/braid/pool.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-after-refusal.txt")
    machine.succeed("cmp /tmp/fi-show-before-replace.txt /tmp/fi-show-after-refusal.txt")


with subtest("Closing the conflicting mapper lets replace proceed"):
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed(replace_cmd())
    pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert "disk2" not in member_names(pool), pool
    assert "disk3" in member_names(pool), pool
    assert machine.succeed("cat /mnt/storage/kept.txt").strip() == "replace cloned header data"


machine.shutdown()
