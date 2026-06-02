---
intent: "Record why braid delegates LUKS sector size to cryptsetup auto-detect, plus the btrfs context behind that choice. Read before changing related behavior or docs."
status: Active
---
# LUKS sector size and btrfs

## Summary

braid does not pass `--sector-size` to `cryptsetup luksFormat`, and it
rejects operator attempts to set it. With the flag omitted, cryptsetup
auto-detects the encryption sector size from each device -- and that
auto-detected value is already the optimal one for the device -- so braid
never chooses a sector size itself.

## What auto-detect picks

When `--sector-size` is omitted, cryptsetup sizes the LUKS2 encryption
sector to the device's physical sector:

> The encryption sector size is set based on the underlying data device if not specified explicitly.
> For native 4096-byte physical sector devices, it is set to 4096 bytes.
> For 4096/512e (4096-byte physical sector size with 512-byte sector emulation), it is set to 4096 bytes.
> For drives reporting only a 512-byte physical sector size, it is set to 512 bytes.
>
> -- cryptsetup 2.8.6, `man/common_options.adoc` (LUKSFORMAT branch)

The rule, in short:

- 4Kn (native 4096) and 512e (4096/512e) drives -> 4096-byte LUKS sectors
- drives reporting a 512-byte physical sector -> 512-byte LUKS sectors

On our hardware:

- **NAS drives**: 8TB+ SATA HDDs (4Kn or 512e) -> **4096-byte** LUKS
  sectors, matching the physical sector.
- **Test drives**: USB sticks and VM virtio disks report 512-byte
  sectors -> **512-byte** LUKS sectors. The committed `luksDump`
  fixtures (`cli/tests/fixtures/nixos-26.05/cryptsetup-luks-dump.json`
  and its `nixos-unstable` mirror) record `"sector_size":512` for
  exactly this reason: they capture VM disks, not the NAS hardware.

## Why braid doesn't override it

Two reasons:

1. **Auto-detect already yields the optimal value.** Setting
   `--sector-size` explicitly could at best re-specify what cryptsetup
   would pick anyway, while adding a format-time parameter that cannot
   change without re-encrypting the device. There is nothing to gain.

2. **An override could make braid's capacity estimate unsafe.** braid
   rejects `--sector-size` passed as a `--luks-format-arg` override (see
   `cli/src/types.rs#LuksFormatExtraOpts::parse`); `replace` lists it
   among the
   [on-disk-layout flags it refuses](../../commands/replace.md#safety-checks--refusal-cases).
   A non-default sector size can shift the fresh-LUKS payload offset, and
   braid's capacity check for a **fresh** target assumes cryptsetup's
   default offset. The scope here is deliberately fresh targets only:
   [the replace-target size preflight](../luks-unlock.md#replace-target-size-preflight)
   covers existing containers, whose capacity is read from the LUKS2
   segment and is exact at any sector size.

## Aside: even 512-byte LUKS sectors are harmless under btrfs

This section covers the 512-byte LUKS sector case -- the test drives
above, and the historical worry that motivated `--sector-size 4096` in
the first place. It does **not** describe the NAS, which gets 4096-byte
LUKS sectors from auto-detect. Even at 512-byte sectors, btrfs sees no
read-modify-write penalty.

### The three layers

```
btrfs (always 4096-byte blocks)
  -> LUKS (512 or 4096-byte sectors)
    -> physical disk (512 or 4096-byte sectors)
```

### Why --sector-size 4096 exists

Read-modify-write amplification happens at the physical disk when
something writes less than a full physical sector. Example: writing a
single 512-byte LUKS sector to a 4096-byte-physical-sector disk forces
the disk to read 4096 bytes, modify 512, and write 4096 back.

### Why btrfs avoids it even at 512-byte LUKS sectors

btrfs never writes anything smaller than 4096 bytes. Take a 4096-byte
btrfs write landing on a LUKS device with 512-byte sectors. dm-crypt
encrypts that write as 8 x 512-byte crypto sectors internally -- but the
internal crypto-sector count is not the I/O count:

> dm-crypt does not split the write -- it allocates one clone bio for the
> entire write and submits it downstream as a single bio:
>
> ```c
> clone = crypt_alloc_buffer(io, io->base_bio->bi_iter.bi_size);
> ```
>
> -- Linux 6.18.33, `drivers/md/dm-crypt.c` (`kcryptd_crypt_write_convert`)

The physical disk therefore receives a full 4096-byte write -- no
read-modify-write penalty. The only overhead is CPU: 8 IV computations
and 8 smaller AES operations instead of 1. With AES-NI doing multiple
GB/s, that is negligible next to spinning-disk speeds.

### When --sector-size 4096 would matter

Filesystems that can issue sub-4096 writes: ext4 with 1K blocks, raw
`dd`, database engines doing 512-byte writes. btrfs is not one of them.
