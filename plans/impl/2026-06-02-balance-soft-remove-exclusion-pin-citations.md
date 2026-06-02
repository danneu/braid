# Plan: tighten `docs/internals/btrfs/balance-soft.md`

## Context

`docs/internals/btrfs/balance-soft.md` now correctly explains the main hard-vs-soft
balance distinction:

- `braid add` of a 3rd-or-later device uses hard RAID1 convert so existing
  raid1 chunks are rewritten and redistributed onto the new device.
- `maybe_restore_raid1` uses soft RAID1 convert to clean up leftover `single`
  chunks after `remove-missing` or missing-path `replace`.
- `recover` resumes owed RAID1 maintenance and then runs an idempotent soft
  catch-up for add / remove-missing / replace maintenance paths.
- The page now includes the important allocation nuance: degraded writes create
  `single` chunks only when fewer than two devices are available for allocation.

Two incremental fixes remain:

1. The recovery section should explicitly say that `remove` recovery is excluded
   from resume-then-soft replay.
2. The existing upstream citation should be pinned to the btrfs-progs version
   braid ships, without duplicating the quoted `soft` sentence.

## Intended outcome

Keep the current page shape and prose. Apply a narrow documentation patch that
adds the `remove` recovery exclusion, pins the existing top-of-page
`btrfs-balance` quote to btrfs-progs `6.19.1`, and replaces drifting external
source links with pinned source pointers in code spans.

## Proposed edits

### 1. Add the `remove` recovery exclusion

In `docs/internals/btrfs/balance-soft.md`, under `## Recover replay`, after the
paragraph that says replay fires for interrupted `add` and owed
`remove-missing` / `replace` post-maintenance, add a short paragraph:

```markdown
`braid remove` is deliberately not part of this replay. It is the only mutation
whose pre-mutation phase can issue a balance -- the RAID1 -> single conversion
in the 2->1 case. A paused balance found while recovering a `remove` may be that
unfinished conversion-to-single, not owed RAID1 maintenance, so recover neither
resumes nor soft-replays it. Resuming it would finish converting to single
without removing the device, then clear the journal, silently halving
redundancy. Recover instead directs the operator to re-run `braid remove`.
```

### 2. Pin the existing upstream citations

In `## What `soft` does`, keep the existing quoted sentence exactly once, but
pin its provenance by replacing:

```markdown
`soft` is a per-type modifier for `convert=` filters. From btrfs-balance(8):
```

with:

```markdown
`soft` is a per-type modifier for `convert=` filters. From btrfs-progs
`Documentation/btrfs-balance.rst` (version `6.19.1`, tag `v6.19.1`, commit
`fa79dbea32d39ac0ae41a88a079013c7ad2a8a58`):
```

Do not add a second copy of the quoted sentence in Sources.

In `## Sources`, replace the first two drifting external bullets:

```markdown
- [btrfs-balance(8) -- soft filter](https://btrfs.readthedocs.io/en/latest/btrfs-balance.html)
- [btrfs-man5 -- RAID profiles](https://btrfs.readthedocs.io/en/latest/btrfs-man5.html)
```

with pinned source pointers as code spans, not markdown links:

```markdown
- btrfs-progs `Documentation/btrfs-balance.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58` -- `soft`
  filter semantics.
- btrfs-progs `Documentation/btrfs-man5.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58` -- degraded
  mounts and mixed block group profiles.
```

Keep the existing braid internal-links bullet unchanged.

## Authoritative anchors

- `cli/src/recover.rs:1526-1538`: `OpKind::Remove` maps to
  `RecoverCompletion::GenericLivePool { replay_raid1_maintenance: false }`,
  with the rationale that a paused remove balance may be the pre-remove
  RAID1 -> single conversion.
- `cli/src/recover.rs:16069-16092`: `recover_skips_paused_balance_resume_for_remove`
  pins that recover does not resume or soft-replay a paused `remove` balance.
- btrfs-progs `Documentation/btrfs-balance.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58`: pinned `soft`
  filter semantics.
- btrfs-progs `Documentation/btrfs-man5.rst`, version `6.19.1`, tag
  `v6.19.1`, commit `fa79dbea32d39ac0ae41a88a079013c7ad2a8a58`: degraded mount
  behavior and mixed profile examples (`Data: single, raid1`).

## Out of scope

- No code, test, fixture, or broader docs changes.
- Do not restructure the page or replace the current hard-vs-soft rewrite.
- Do not change `docs/SUMMARY.md`.

## Verification

Doc-only change; no Rust or VM tests are required.

1. `mdbook build docs` -- must pass.
2. `rg -ni "readthedocs|en/latest" docs/internals/btrfs/balance-soft.md` --
   expect no matches.
3. `rg -n "Documentation/btrfs-balance.rst|Documentation/btrfs-man5.rst|6\\.19\\.1|v6\\.19\\.1|fa79dbea32d39ac0ae41a88a079013c7ad2a8a58" docs/internals/btrfs/balance-soft.md`
   -- expect matches proving the pinned upstream provenance is present.
4. `rg -ni "remove.*not part|pre-mutation phase|soft-replays|re-run .*braid remove" docs/internals/btrfs/balance-soft.md`
   -- expect matches proving the remove exclusion is documented.
5. `rg -c "are left untouched" docs/internals/btrfs/balance-soft.md`
   -- expect exactly one match, so the upstream quote is not duplicated.
6. Re-read the final page against the anchors above and confirm the recovery
   replay wording does not imply `remove` participates in
   `replay_owed_raid1_maintenance`.
