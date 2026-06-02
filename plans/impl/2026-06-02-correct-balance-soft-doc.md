# Fix `balance-soft.md`: it claims braid never uses soft balance (it does)

## Context

`docs/internals/btrfs/balance-soft.md` is a maintainer-facing internals page
whose central section, **"Why we don't use it"**, is false. braid *does* issue
`btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft` (`pool_balance_raid1_soft` /
`BtrfsBalanceRaid1Soft`) -- both in its live post-degraded redundancy-restore
path (`maybe_restore_raid1`) and in recover's replay of owed RAID1 maintenance
(`replay_owed_raid1_maintenance`). A maintainer reading this page would conclude
the only balance path is the hard `BtrfsBalanceRaid1` and could delete the soft
path or write the wrong recovery test.

The page is stale, not wrong-by-design: it was written `7546ee66` (2026-02-25),
the soft code landed `443e1e39` (2026-03-12, 15 days later), and the docs-tree
move `403d1b07` (2026-05-22) carried the content over unrevised.

The same page carries a **second** stale claim (same root cause): it describes
the replace workflow as "add new -> balance -> remove old". braid replace uses
`btrfs replace start` (atomic); ADR-001 already records add+balance+remove as a
*rejected* alternative.

**Outcome:** the page should accurately state where braid uses hard convert vs
soft convert, and why, anchored to the code paths and authoritative btrfs docs.

## Scope

Single file: `docs/internals/btrfs/balance-soft.md`. No other doc repeats either
stale claim -- every adjacent doc is already correct and consistent
(`docs/design/principles.md:21-22`, `docs/commands/remove-missing.md:66`,
`docs/commands/replace.md:95`, `docs/commands/add.md:91`,
`docs/design/decisions/001-btrfs-raid1.md:46-50`). This is a content correction
plus cross-links, not a sweep. The action is **docs**; no code changes -- the
code is correct.

## The real distinctions to document

**Hard vs soft convert.**

- **Hard convert -- `braid add` of a 3rd+ device.** Every chunk is already
  raid1, so `soft` would skip them all (no-op) and leave the new device empty;
  only a hard rewrite redistributes copies onto it. (The existing A/B/C no-op
  analysis on the page is correct and should be kept under this heading.)
- **Soft convert -- converting leftover `single` chunks.** btrfs writes a
  `single` chunk only when it cannot place two copies, i.e. a RAID1 pool with
  fewer than two devices present for allocation (the 2-disk-one-survivor case;
  a 3-disk pool degraded to two stays raid1 -- `degraded-writes-3disk.py`).
  Soft converts exactly those `single` chunks and skips already-raid1 ones, so
  it is idempotent and cheap -- safe to run as cleanup even when nothing needs
  converting.

**Three paths use the soft balance, not one.** The page must not teach that
`BtrfsBalanceRaid1Soft` exists only for post-degraded restore:

1. Live restore via `maybe_restore_raid1` -- `remove-missing` and `replace`
   (missing path), after clearing the last missing device.
2. Recover replay via `replay_owed_raid1_maintenance` -- resumes a paused
   balance, then runs the soft balance on 2+ device pools, for an interrupted
   `add` and for owed `remove-missing` / `replace` post-maintenance.

### Verified anchors (use these; do not re-derive)

- soft semantics -- `reference/btrfs-progs/Documentation/btrfs-balance.rst:273-274`:
  "When doing convert from one profile to another and soft mode is on, chunks
  that already have the target profile are left untouched." (per-type, line 278).
- hard-for-add rationale already in code -- `cli/src/add.rs:776-781` comment:
  "HARD convert, not ,soft ... only a hard rewrite redistributes copies onto the
  new device; ,soft would skip them all and leave it empty. A 1->2 add converts
  existing single chunks either way."
- degraded-write mechanism -- `cli/src/pool.rs:455-457` doc comment:
  "restores redundancy for single-profile chunks created during degraded
  operation (known btrfs bug)." NUANCE: `single` chunks appear only when fewer
  than two devices are present for allocation. Proven both ways by repro tests:
  `tests/repro/degraded-writes-single.py` (2-disk, one survivor -> creates
  `single`) and `tests/repro/degraded-writes-3disk.py:88-90` (3-disk, two
  survivors -> stays `raid1`, asserts no `single`). Do not write "degraded
  writes create single" unconditionally.
- soft is idempotent cleanup -- `cli/src/recover.rs:16010-16014` test comment:
  the post-add replay is "unconditional ... Idempotent: the `,soft` filter skips
  already-RAID1 chunks." This is why braid runs it without first checking for
  `single` chunks.
- soft code path -- `cli/src/cmd.rs:681-691` (`BtrfsBalanceRaid1Soft`),
  `cli/src/pool.rs:407-428` (`pool_balance_raid1_soft`). Two entry points:
  (1) `maybe_restore_raid1` (`cli/src/pool.rs:462-495`), invoked
  `cli/src/remove_missing.rs:279` and `cli/src/replace.rs:916` (missing path);
  (2) `replay_owed_raid1_maintenance` (`cli/src/recover.rs:1801-1853`).
- recover replay -- `replay_owed_raid1_maintenance` (`cli/src/recover.rs:1801-1853`)
  resumes a paused balance via `pool_balance_resume` (`btrfs balance resume`),
  then runs `pool_balance_raid1_soft` on pools with >=2 devices. Enabled for the
  interrupted-`add` recovery at `cli/src/recover.rs:1519-1525`
  (`replay_raid1_maintenance: true`, called `:1157-1158`, label `"add"`); also
  called for the add balance finish (`:2279`, `"add"`) and the owed
  post-maintenance paths of `remove-missing` (`:2788`) and `replace` (`:3273`),
  each gated on `restore_raid1_after_commit`.
- hard code path -- `cli/src/add.rs:1522` -> `pool_balance_raid1`
  (`cli/src/pool.rs:358`) -> `BtrfsBalanceRaid1` (`cli/src/cmd.rs:670`).
- replace uses `btrfs replace start` -- `cli/src/replace.rs:351`
  (`BtrfsReplaceStart`); rejected-alternative recorded in
  `docs/design/decisions/001-btrfs-raid1.md:46-50`.

Note the recover replay first **resumes** any paused balance (which drains the
kernel-persisted convert filters of the original op, e.g. a 3rd+ `add`'s hard
balance) and only then runs the soft pass for `single`-chunk cleanup. The soft
pass is not claimed to complete a cancelled 3rd+ redistribution -- scope the
prose to redundancy/`single`-chunk cleanup, not redistribution.

## Proposed rewritten page

Replace the body wholesale (the rewrite touches most prose anyway). Use ASCII
`--`, not em-dashes, per project style.

````markdown
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
````

## What changes vs. what stays

- **Stays (accurate):** the "What it does" intro and the A/B/C no-op analysis --
  reused, re-homed under the new hard-convert heading.
- **Replaced:** the "Why we don't use it" heading and its false premise; the
  stale "replace workflow (add new -> balance -> remove old)" parenthetical.
- **Reframed (same root cause):** the old "When it helps" section implied soft
  is braid's resume mechanism. Replaced with an accurate "Recover replay"
  section -- recover resumes a paused balance with `btrfs balance resume`, then
  runs the soft balance as idempotent `single`-chunk cleanup. This also
  documents the soft balance's recover caller
  (`replay_owed_raid1_maintenance`: interrupted add + owed post-maintenance), so
  the page no longer implies soft exists only for post-degraded restore.
- **Corrected:** the `single`-chunk allocation model. The first draft said
  degraded writes create `single` unconditionally; in fact that only happens
  with fewer than two present devices (2-disk-one-survivor). The soft balance is
  framed as idempotent cleanup, not an always-necessary rewrite.
- **Added:** cross-links to ADR-001, principles, and the command docs.

## Relative-link sanity (linkcheck will enforce)

From `docs/internals/btrfs/balance-soft.md`, `../../` reaches `docs/`:
`../../design/decisions/001-btrfs-raid1.md`, `../../design/principles.md`,
`../../commands/replace.md`, `../../commands/remove-missing.md`.

## Verification

1. `mdbook build docs` -- must pass; `mdbook-linkcheck2` (configured in
   `docs/book.toml`) fails CI on any broken cross-link, so this validates the
   four new internal links.
2. Re-read the rewritten page against the verified anchors above -- confirm
   every code-path reference (`pool_balance_raid1`, `pool_balance_raid1_soft`,
   `maybe_restore_raid1`, `replay_owed_raid1_maintenance`, `pool_balance_resume`,
   `BtrfsBalanceRaid1` / `BtrfsBalanceRaid1Soft`, the `remove_missing.rs` /
   `replace.rs` / `recover.rs` call sites) still resolves. Cross-check the
   `single`-chunk allocation nuance against `tests/repro/degraded-writes-single.py`
   (2-disk, creates `single`) and `tests/repro/degraded-writes-3disk.py`
   (3-disk, stays `raid1`).
3. No code or tests change; no `just test-*` run is required. (Optional: the
   per-page docs-review command added in `8ae0c26d` can re-audit the page.)

## Implementation notes

- The plan's "Relative-link sanity (linkcheck will enforce)" premise was wrong
  about repo reality. `just check-docs` (a CI gate in
  `.github/workflows/docs.yml`) rejected *any* `](../../` link via a coarse
  grep, so the four in-book cross-links in the rewritten page would have failed
  CI -- the grep is an over-broad false positive (a depth-2 page legitimately
  needs `../../` to reach the docs root, and `mdbook-linkcheck2` already
  validates that in-book targets exist). Per user direction, the grep was
  replaced with a depth-aware checker (`scripts/docs/check-doc-link-escapes.py`,
  wired into `check-docs`) that flags only links whose normalized target climbs
  above `docs/`. This keeps the page's in-book links exactly as the plan wrote
  them.
- Scope expanded beyond the plan's "single file / no code changes" (added the
  checker script plus a `justfile` edit) with explicit user approval, because
  the plan's link approach could not pass CI as written.
- Side effect: the pre-existing `../../` link at
  `docs/design/decisions/010-toolchain-pinning.md:51` -- the sole reason
  master's `check-docs` was already red -- is a valid in-book link, so the
  depth-aware check now accepts it and this change turns that gate green.
- Follow-up (`0d22fb76`): the depth-aware checker above was removed as
  redundant. `mdbook-linkcheck2` already forbids links escaping the book root
  and validates in-book targets (verified empirically: an escaping link fails
  the build with "Linking outside of the 'root' directory is forbidden" even
  when the target exists), and the SUMMARY.md-parity check guarantees every
  page is rendered -- so `check-docs` no longer link-checks at all. The page's
  in-book links stand; linkcheck2 is the gate. The pre-existing
  `010-toolchain-pinning.md` link still passes for the same reason.
