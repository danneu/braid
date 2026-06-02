---
intent: "Record btrfs balance profile conversions and block group types context for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# btrfs balance: profile conversions and block group types

## Block group types

btrfs has three block group types, each with an independent RAID profile:

| Type | Contents | Default (1 device) | Default (2+ devices) |
|------|----------|---------------------|----------------------|
| **data** | File contents | single | single |
| **metadata** | Inodes, directory entries, extent tree | dup | raid1 |
| **system** | Chunk tree (maps virtual → physical addresses) | dup | raid1 |

## System chunks follow metadata automatically

When converting profiles with `btrfs balance start -mconvert=<profile>`,
system chunks are converted alongside metadata. You do **not** need to pass
`-sconvert=<profile>` separately.

The `-s` flag exists for converting system chunks *independently* of metadata,
which requires `-f` because btrfs considers it dangerous.

This means our standard conversion commands are complete:

```sh
# single → RAID1 (after adding 2nd device)
btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage

# RAID1 → single (before removing last redundant device)
btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage
```

Note the asymmetry in the second command: data converts to `single`, but
metadata converts to **`dup`**, not `single`. A one-device pool keeps two
same-disk copies of metadata (and system chunks), matching what
`mkfs.btrfs` lays down for a fresh single-device filesystem (see the table
above). `-mconvert=single` would leave metadata with a single unprotected
copy. The `-f` is required here because reducing metadata from RAID1 to
`dup` lowers redundancy -- btrfs refuses that without a force flag -- not
because of the `-s` independent-conversion case discussed above.

## Why system chunks matter

System chunks contain the chunk tree — the structure that maps virtual
addresses to physical device locations. Losing the only copy of a system
chunk usually means losing the entire filesystem. On a multi-device pool,
having system chunks in RAID1 ensures this map survives a single device
failure.

## Sources

- [Balance — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Balance.html)
- [btrfs-balance(8)](https://btrfs.readthedocs.io/en/latest/btrfs-balance.html)
- [linux-btrfs: safe/necessary to balance system chunks?](https://linux-btrfs.vger.kernel.narkive.com/CfTvubeh/safe-necessary-to-balance-system-chunks)
