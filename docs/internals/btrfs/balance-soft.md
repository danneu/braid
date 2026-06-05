---
intent: "Record where braid uses hard vs `soft` RAID1 convert-balance, and why. Read before changing balance behavior or the `maybe_restore_raid1` path."
status: Active
---
# btrfs balance: the `soft` flag

## What `soft` does

`soft` is a per-type modifier for `convert=` filters. From btrfs-progs
`Documentation/btrfs-balance.rst` (version `6.19.1`, tag `v6.19.1`, commit
`fa79dbea32d39ac0ae41a88a079013c7ad2a8a58`):
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

### Skip -- degraded add (missing member present)

`braid add` into a pool that still has a missing member runs NO convert
balance at all. The post-add present-device count can already be `>= 2` (a
2-disk RAID1 with one member missing, plus the fresh disk), which would
otherwise trip the hard convert above; braid gates it off on
`missing_count > 0` and surfaces a single `[skip]` note instead. The skip is
applied symmetrically in `cli/src/add.rs`: `plan_add` pushes one
`PreviewNote::Skip`, and the preview step builder (`AddWorkPlan::render_steps`)
and the execute balance gate (`AddPlan::execute`) both carry the same
`missing_count == 0` condition so dry-run and real-run agree.

This is a deliberate **deferral**, not a hazard fix. The hard convert does
succeed on a degraded pool today -- `btrfs device add` works on a degraded
mount and the convert rewrites every chunk across the present devices -- but it
rewrites *all* data through the allocator while the pool has no redundancy, a
longer and less-targeted operation than the purpose-built `btrfs replace`.
braid instead defers redundancy restoration to the repair step:
`remove-missing` (which relocates data onto the new disk and runs the soft
balance above) or `replace`. The soft convert, by contrast, is left running
even on a degraded pool -- it only converts `single` -> `raid1` and never
rewrites existing `raid1` chunks, so it cannot do a full degraded rewrite and
is safe and beneficial there.

Skipping at add also makes the degraded-add interrupt paths converge. With no
hard balance issued, a *completed* degraded add and *every* recover path end at
the same state: device added, pool still degraded, redundancy deferred to the
repair step. Before this change the paths diverged: a completed degraded add
restored redundancy via the hard balance, but recover could only safely replay
owed RAID1 maintenance when no paused balance survived the interruption.
Skipping at add closes that divergent path by making degraded-add recovery end
in the same deferred-repair state.

btrfs-progs guidance backs the deferral. `btrfs-balance.rst` (in Sources)
recommends you "use :command:`btrfs replace` or :command:`btrfs device remove`
to handle the failing/missing device first." We lean on that as general
guidance, not a strong prohibition -- its acute warning is narrower, about
converting to a profile with *lower* redundancy (RAID1 -> SINGLE) with a
present-but-failing device, milder than our convert *to* `raid1` with a
cleanly-missing member.

## Recover replay

After a forced shutdown mid-mutation, `braid recover` replays owed RAID1
maintenance only if `btrfs balance status` reports no active balance:

> [!WARNING]
> Replaying a crash-paused RAID1 balance can underflow btrfs block-group accounting and silently halve redundancy. recover preserves `pending-op.json` instead of automating recovery when the balance state is paused, running, or unknown.

On any pool with two or more devices, the idle/no-paused path runs the soft
balance above to catch `single` chunks an interrupted balance left behind. The
idempotent `,soft` filter makes this safe even when nothing needs converting.

This replay fires for an interrupted `add` when the balance state is idle -- the
new disk is already in the pool, so re-running `braid add` would refuse, and
recover finishes the job so the operator is not left with `single` chunks -- and
for the idle/no-paused owed post-maintenance step of `remove-missing` and
`replace`.

`braid remove` is deliberately not part of this replay. It is the only mutation
whose pre-mutation phase can issue a balance -- the RAID1 -> single conversion
in the 2->1 case. A paused balance found while recovering a `remove` may be that
unfinished conversion-to-single, not owed RAID1 maintenance, so recover neither
resumes nor soft-replays it. Resuming it would finish converting to single
without removing the device, then clear the journal, silently halving
redundancy. Recover instead directs the operator to re-run `braid remove`.

## Sources

- btrfs-progs `Documentation/btrfs-balance.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58` -- `soft`
  filter semantics.
- btrfs-progs `Documentation/btrfs-man5.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58` -- degraded
  mounts and mixed block group profiles.
- braid: [ADR-001 btrfs RAID1](../../design/decisions/001-btrfs-raid1.md) (replacement strategy, add+balance+remove rejected), [design principles](../../design/principles.md) (degraded restore), and the [`replace`](../../commands/replace.md) / [`remove-missing`](../../commands/remove-missing.md) command docs.
