---
intent: "Record LUKS sector size and btrfs context for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# LUKS sector size and btrfs

## Summary

LUKS `--sector-size 4096` is irrelevant for braid. btrfs always writes
4096-byte (or larger) blocks, which eliminates the I/O amplification
that `--sector-size 4096` is designed to prevent. We use the default
512-byte LUKS sector size.

## The three layers

```
btrfs (always 4096-byte blocks)
  → LUKS (512 or 4096-byte sectors)
    → physical disk (512 or 4096-byte sectors)
```

## Why --sector-size 4096 exists

Read-modify-write amplification happens at the physical disk when
something writes less than a full physical sector. Example: writing a
single 512-byte LUKS sector to a 4096-byte physical sector disk forces
the disk to read 4096 bytes, modify 512, write 4096 back.

## Why it doesn't matter for btrfs

btrfs never writes anything smaller than 4096 bytes. A 4096-byte btrfs
write with 512-byte LUKS sectors:

1. dm-crypt encrypts the data in 8 × 512-byte chunks internally
2. dm-crypt does NOT split the I/O — it forwards the original 4096-byte
   bio to the disk as a single write
3. The physical disk receives a full 4096-byte write — no
   read-modify-write penalty

The only overhead is CPU: 8 IV computations and 8 smaller AES operations
instead of 1. With AES-NI doing multiple GB/s, this is negligible
compared to spinning disk speeds.

## When --sector-size 4096 would matter

Filesystems that can do sub-4096 writes (ext4 with 1K blocks, raw `dd`,
database engines doing 512-byte writes). btrfs is not one of them.

## Our hardware

- **NAS drives**: 8TB+ SATA HDDs, almost certainly 4096-byte physical
  sectors (4Kn or 512e). btrfs prevents the amplification issue anyway.
- **Test drives**: USB sticks with 512-byte sectors. `--sector-size 4096`
  would work fine (LUKS sector size is a logical abstraction, not tied to
  physical sector size), but there's no benefit.

## Decision

Don't set `--sector-size 4096` on `luksFormat`. It adds complexity
(format-time parameter that can't be changed without re-encryption) for
zero benefit with btrfs.
