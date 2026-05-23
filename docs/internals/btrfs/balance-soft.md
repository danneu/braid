---
intent: "Record btrfs balance the `soft` flag context for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# btrfs balance: the `soft` flag

## What it does

`soft` is a per-type modifier for `convert=` filters. It tells balance to skip
block groups already tagged with the target profile.

```sh
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage
```

Without `soft`, every block group is rewritten regardless of its current
profile. With `soft`, only block groups that don't match the target are touched.

## When it helps

Resuming an interrupted profile conversion. If a `single → raid1` balance gets
halfway through and is restarted, `soft` skips the chunks already converted to
raid1 and only processes the remaining single chunks. Without it, already-
converted chunks are rewritten from scratch — wasted I/O.

## Why we don't use it

`soft` only looks at the profile tag, not at data distribution. A block group
tagged `raid1` is skipped even if all its copies live on a subset of devices.

This breaks the 3rd-device-add case:

1. Pool has devices A, B — all chunks are raid1 across A and B.
2. Add device C.
3. `balance start -dconvert=raid1,soft` — every chunk is already raid1, so
   `soft` skips them all. Balance is a no-op.
4. Device C sits empty. Existing data has zero copies on C.

Without `soft`, the balance rewrites every chunk, redistributing copies across
all three devices. That redistribution is the whole point of running balance
after adding a device to an existing raid1 pool.

For our replace workflow (add new → balance → remove old), the subsequent
`device remove` forces redistribution anyway, so `soft` would be harmless
there. But `braid add` of a 3rd+ disk goes through the same `BtrfsBalanceRaid1`
code path, where `soft` would silently make the balance a no-op.

Using `soft` only on checkpoint-resume and not on fresh starts would require
conditional logic for a niche case (interrupted balance). The cost of not having
`soft` is just redundant I/O on restart — not a correctness issue.

## Sources

- [btrfs-balance(8) — soft filter](https://btrfs.readthedocs.io/en/latest/btrfs-balance.html)
- [btrfs-man5 — RAID profiles](https://btrfs.readthedocs.io/en/latest/btrfs-man5.html)
