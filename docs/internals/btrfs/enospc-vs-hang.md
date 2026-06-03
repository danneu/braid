---
intent: "Record ENOSPC vs hang reproducing btrfs device remove failures in VMs context for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# ENOSPC vs hang: reproducing btrfs device remove failures in VMs

## Background

`btrfs device remove missing` has two failure modes when surviving devices
lack space for relocation. Both are bad, but the second is catastrophic.

## Failure mode 1: instant ENOSPC

**Conditions:** surviving devices have zero (or near-zero) unallocated space.

btrfs can't even begin relocating block groups. It fails immediately:

```
ERROR: error removing device 'missing': No space left on device
```

Filesystem stays healthy and writable. Annoying but recoverable.

**How to reproduce in a VM:** 3×512MiB disks, fill to 100% capacity, kill
one disk. `btrfs device remove missing` fails in under a second.

## Failure mode 2: partial relocation → transaction abort → forced read-only

**Conditions:** surviving devices have SOME unallocated space (hundreds of
MiB) but not enough to relocate ALL block groups from the dead device.

btrfs starts relocating, successfully moves some block groups (consuming the
free space), then hits ENOSPC mid-transaction on a subsequent block group.
The transaction abort forces the entire filesystem read-only:

```
BTRFS info: relocating block group 4761583616 flags data|raid1
BTRFS info: found 20 extents, stage: move data extents
BTRFS info: found 20 extents, stage: update data pointers
BTRFS info: relocating block group 3419406336 flags metadata|raid1
BTRFS: Transaction aborted (error -28)
BTRFS: error in __btrfs_free_extent: errno=-28 No space left
BTRFS info: forced readonly
```

The error reported to the user is "Read-only file system" — the ENOSPC is
buried in dmesg. The filesystem is destroyed (forced read-only) and requires
remounting or rebooting to recover.

On real hardware with slow USB drives, btrfs doesn't crash quickly — it
spends hours doing I/O, throttled by writeback queuing (`wbt_wait`), retrying
the same block groups before eventually aborting. In a VM with fast virtual
disks, the same sequence completes in ~40 seconds.

## What makes the difference

The variable is whether btrfs can **begin** relocating:

| Free space on survivors | btrfs behavior | Outcome |
|------------------------|----------------|---------|
| ~0 | Can't start → instant ENOSPC | Filesystem OK |
| Some but not enough | Starts, partially succeeds, then ENOSPC mid-transaction | **Filesystem destroyed** (forced read-only) |
| Enough | Completes relocation | Success |

The dangerous middle case is the one that happened in the real incident
(3×8GiB USB drives, ~80% full, one died).

## How braid avoids this

braid's mutation preflight refuses these removals — before the pending-op
journal is written — whenever it can *prove* the survivors lack the space to
absorb the target's allocations. The degraded failure-mode-2 path is fully
guarded: `remove-missing` and the 2→1 eviction are fail-closed, so an
operator using braid does not reach the catastrophic path above. The
healthy >=2-survivor case is intentionally warn-and-proceed on an
*unprovable* check, because it falls through to btrfs's clean failure mode
1, never the mode-2 abort. Per path:

- **`remove-missing`** — the degraded failure-mode-2 scenario exactly.
  Computes RAID1 chunk-pair capacity on the survivors and refuses when it is
  below the chunks allocated on the missing device. **Fail-closed:** any
  probe or parse uncertainty also refuses
  (`cli/src/preflight.rs::check_raid1_relocation_space`, wired in
  `cli/src/remove_missing.rs`).
- **`remove` evicting to a single survivor (2→1)** — RAID1 no longer
  applies, so braid instead checks the lone survivor can hold the
  post-conversion `data + 2 × metadata + 2 × system` (single + DUP profile).
  **Fail-closed** (`cli/src/preflight.rs#check_single_survivor_capacity`).
  Enforced at plan time *and* re-validated as a pre-journal gate in
  `cli/src/remove.rs#RemovePlan::execute`, closing the drift window where the
  pool keeps taking writes while the operator idles at the confirmation
  prompt — an over-committed survivor is then refused before the irreversible
  `-f` balance, still with no `pending-op.json` stranded.
- **`remove` with >=2 survivors (healthy)** — same RAID1 relocation check,
  but **warn-and-proceed** on probe/parse uncertainty. A best-effort miss
  here falls through to `btrfs device remove`, which hits the *clean*
  failure mode 1 (instant ENOSPC), not the failure-mode-2 abort, so the
  filesystem stays intact.
- **`replace` is not subject to this failure mode.** `btrfs replace`
  rebuilds onto the new disk instead of relocating onto survivors; its
  preflight refuses a new disk smaller than the one being replaced
  (`cli/src/preflight.rs::check_replace_target_capacity`).

`braid status` and `braid doctor` surface a proactive advisory
(`cli/src/capacity.rs::enospc_risk_advisory`) one disk-loss *before* a pool
enters this danger zone.

The policy and its rationale are owned by ADR 012's "ENOSPC pre-flight
check" section (`docs/design/decisions/012-intent-cli.md`). See also
`docs/commands/remove-missing.md` and the `braid status` ENOSPC advisory
(`docs/commands/status.md`).

## Reproducing the hang/crash in a VM

The tricky part is getting btrfs to land in the "some but not enough" zone.
Two challenges:

### 1. btrfs allocates unevenly across devices

Writing 2GiB of data to a 3-device RAID1 pool doesn't give you ~667MiB
allocated per device. btrfs allocates block groups in pairs (for RAID1),
and the pair selection isn't perfectly balanced. In testing:

```
disk1: Unallocated  1.00MiB    ← nearly full
disk2: Unallocated  288.88MiB  ← some room
```

With one device at ~0 free, btrfs can't relocate anything there → instant
ENOSPC (failure mode 1). To get failure mode 2, BOTH survivors need
meaningful free space.

### 2. Block group granularity

btrfs allocates space in block groups (256MiB on small devices, 1GiB on
large ones). A single `dd` write of 200MiB might or might not trigger a new
block group allocation. Writing in smaller chunks (50MiB) gives btrfs more
allocation decisions, improving the chance of even distribution.

### Working recipe (what the test does)

1. **Use 4GiB disks** — large enough for btrfs to create multiple data block
   groups per device, giving room for partial relocation.

2. **Adaptive fill with small chunks** — write 50MiB at a time, check
   `btrfs device usage --raw` after each write, stop when the minimum
   unallocated across all online devices drops below 800MiB. This targets
   the sweet spot: both survivors have 300-800MiB free.

3. **Use `--raw` for parsing** — `btrfs device usage` displays values in
   human units (MiB, GiB) depending on magnitude. `--raw` gives bytes,
   avoiding unit-parsing bugs.

4. **Kill disk3, mount degraded, attempt `btrfs device remove missing`** —
   btrfs starts relocating, succeeds on one block group (~38s of I/O in
   VM), then crashes on the next with transaction abort.

### What didn't work

- **512MiB disks filled to 100%**: instant ENOSPC (failure mode 1). No free
  space for btrfs to even begin.

- **2GiB disks with 200MiB write chunks**: uneven allocation left one
  survivor with 1MiB free → instant ENOSPC again.

- **2GiB disks with adaptive fill**: same uneven allocation problem. Not
  enough total capacity for btrfs to distribute block groups evenly across
  3 device pairs.

- **Parsing `btrfs device usage` without `--raw`**: values display as MiB or
  GiB depending on size. On fresh 4GiB disks, unallocated shows as GiB; a
  regex matching only MiB found zero values → fill loop stopped immediately.

## Test files

- `tests/repro/btrfs-remove-enospc.nix/.py` — failure mode 1 (instant ENOSPC, 3×512MiB)
- `tests/repro/btrfs-remove-enospc-crash.nix/.py` — failure mode 2 (partial relocation crash, 3×4GiB)

These are repro tests that document actual btrfs behavior, not TDD tests.
They assert the real outcomes: instant ENOSPC with surviving filesystem, or
transaction abort with forced read-only. They invoke raw `btrfs device
remove missing` rather than braid precisely because braid's preflight (see
"How braid avoids this" above) refuses the operation under these conditions —
reproducing the unguarded btrfs behavior requires bypassing it. They live in
`tests/repro/` — a folder reserved for tests that reproduce real-world
scenarios for our records.
