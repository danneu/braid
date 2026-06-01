# Test: mkfs-block-group-tree
#
# Intent: Verify braid creates btrfs pools with the `block-group-tree`
# feature bit set on both single-disk and raid1 layouts.
#
# Why it exists: braid requests the `block-group-tree` bit explicitly at mkfs
# time so the on-disk feature set never depends on the linked btrfs-progs
# default. The bit is the btrfs-progs 6.19 default that braid's pinned
# nixos-26.05 toolchain ships; this fails closed if it is ever absent. The rest
# of the feature set still tracks btrfs-progs defaults; ADR-027.
#
# Scenario: Boot a VM, run `braid add` to bootstrap a fresh pool, then inspect
# `btrfs inspect-internal dump-super` on the underlying mapper device(s).
# Fails if BLOCK_GROUP_TREE is missing from compat_ro_flags.

start_all()
single.wait_for_unit("multi-user.target", timeout=120)
raid1.wait_for_unit("multi-user.target", timeout=120)

with subtest("single-disk pool has block-group-tree set"):
    single.succeed(
        "echo -n 'testpassphrase' | braid add "
        "disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes"
    )
    single.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk1 "
        "| grep -q BLOCK_GROUP_TREE"
    )

with subtest("raid1 pool has block-group-tree set on both devices"):
    raid1.succeed(
        "echo -n 'testpassphrase' | braid add "
        "disk1=/dev/disk/by-id/virtio-disk1 "
        "disk2=/dev/disk/by-id/virtio-disk2 "
        "--passphrase-stdin --yes"
    )
    raid1.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk1 "
        "| grep -q BLOCK_GROUP_TREE"
    )
    raid1.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk2 "
        "| grep -q BLOCK_GROUP_TREE"
    )
