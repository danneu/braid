---
intent: "Record where braid uses hard vs `soft` RAID1 convert-balance, and why. Read before changing balance behavior or the `maybe_restore_raid1` path."
status: Active
---
# btrfs balance: the `soft` flag

## What `soft` does

`soft` is a per-type modifier for `convert=` filters. From btrfs-balance(8):
"When doing convert from one profile to another and soft mode is on, chunks that
already have the target profile are left untouched."

```sh
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage
```

Without `soft`, every block group is rewritten regardless of its current
profile. With `soft`, only block groups whose profile differs from the target
are touched. The switch is per-type, so `-dconvert` and `-mconvert` apply it
independently.

`soft` keys on the profile tag alone, not on data distribution: a chunk tagged
`raid1` is skipped even if both copies happen to live on a subset of the
devices. That distinction is exactly why braid uses hard convert in one place
and soft in another.

## Where braid uses hard vs soft

braid issues two different RAID1 convert-balances. The choice of `soft` is
deliberate in each.

### Hard convert -- growing the pool (`braid add`, 3rd+ device)

`braid add` of a 3rd-or-later device runs a HARD `-dconvert=raid1`
(`pool_balance_raid1`, emitting `BtrfsBalanceRaid1`). Soft would be wrong here:

1. Pool has devices A, B -- all chunks are raid1 across A and B.
2. Add device C.
3. `-dconvert=raid1,soft` -- every chunk is already raid1, so `soft` skips them
   all. Balance is a no-op.
4. Device C sits empty. Existing data still has zero copies on C.

A hard rewrite rewrites every chunk, redistributing copies across all three
devices -- which is the whole point of balancing after a device add. (A 1->2 add
converts the existing `single` chunks either way, so the distinction only bites
at the 3rd+ device.)

### Soft convert -- converting leftover `single` chunks

btrfs allocates a `single` chunk (one copy) only when it cannot place two copies
on two devices -- i.e. when a RAID1 pool has fewer than two devices present for
allocation. The common case is a 2-disk pool mounted degraded on its one
surviving device: new writes land as `single`. A larger pool that still has two
survivors keeps allocating `raid1` -- a 3-disk pool degraded to two creates no
`single` chunks -- so this conversion is only ever needed for chunks written
while fewer than two devices were available.

Once the pool is whole again, those `single` chunks must be converted back to
`raid1` to restore redundancy. braid runs a SOFT `-dconvert=raid1,soft`
(`pool_balance_raid1_soft`, emitting `BtrfsBalanceRaid1Soft`): it converts
exactly the `single` chunks and skips everything already `raid1`. Because soft
skips matching chunks, the balance is idempotent and cheap -- a near no-op when
there is nothing to convert -- so braid runs it as cleanup without first
checking whether any `single` chunks exist.

braid issues this soft balance from two code paths:

- **Live restore** -- `maybe_restore_raid1` (`cli/src/pool.rs`), invoked by
  `remove-missing` and by `replace`'s missing path once the operation clears the
  last missing device.
- **Recover replay** -- `replay_owed_raid1_maintenance` (`cli/src/recover.rs`),
  described below.

`replace` itself uses `btrfs replace start` (atomic), not add+balance+remove
(see ADR-001), so this soft balance is the only convert-balance in the replace
path.

## Recover replay

After a forced shutdown mid-mutation, `braid recover` replays owed RAID1
maintenance:

1. If a balance is paused, resume it with `btrfs balance resume`
   (`pool_balance_resume`). This drains the convert filters the kernel
   persisted -- it is not a fresh balance.
2. Then, on any pool with two or more devices, run the soft balance above to
   catch `single` chunks an interrupted balance left behind -- including the
   case where `umount` cancelled (rather than paused) a partial balance. The
   idempotent `,soft` filter makes this safe even when nothing needs converting.

This replay fires for an interrupted `add` -- the new disk is already in the
pool, so re-running `braid add` would refuse, and recover finishes the job so
the operator is not left with `single` chunks -- and for the owed
post-maintenance step of `remove-missing` and `replace`.

## Sources

- [btrfs-balance(8) -- soft filter](https://btrfs.readthedocs.io/en/latest/btrfs-balance.html)
- [btrfs-man5 -- RAID profiles](https://btrfs.readthedocs.io/en/latest/btrfs-man5.html)
- braid: [ADR-001 btrfs RAID1](../../design/decisions/001-btrfs-raid1.md) (replacement strategy, add+balance+remove rejected), [design principles](../../design/principles.md) (degraded restore), and the [`replace`](../../commands/replace.md) / [`remove-missing`](../../commands/remove-missing.md) command docs.
