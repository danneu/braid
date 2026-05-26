# Plan: clarify `braid add` behavior on a degraded (missing-device) pool

## Context

`docs/commands/add.md:110` lists, under "Safety checks / refusal cases":

> - Warns if the pool has missing devices (suggests `braid replace` first)

That bullet is technically true but incomplete, and it sits under a heading
whose other bullets are refusals ("Rejects...", "Refuses..."). An operator
reading only the doc can reasonably assume a missing device *blocks* the add.
It does not. Verified behavior:

- On `missing_count > 0`, add emits a warning note and **proceeds** -- no
  refusal (`cli/src/add.rs:1722-1726`).
- A RAID1 balance is planned when `total_after >= 2`, where
  `total_after = pool.devices.len() + mapper_paths.len()`
  (`cli/src/add.rs:1477-1492`, preview steps at `:762-771`). Because
  `pool.devices` **excludes** btrfs-MISSING members
  (`cli/src/probe.rs:486`, pinned by `probe_pool_degraded_missing_sentinel`,
  `cli/src/probe.rs:1620` "MISSING device must be excluded"), a 1-present +
  1-missing pool plus one
  fresh disk reaches `total_after == 2`. At execution the balance is reached
  only if `btrfs device add` succeeds first: `pool_add_device` returns `Err` on
  a nonzero device-add (`cli/src/pool.rs:235-246`), propagated by the `?` at
  `cli/src/add.rs:1420` before the balance. Whether a degraded add succeeds is
  btrfs-dependent.
- The balance is a **hard** convert -- `btrfs balance start --enqueue
  -dconvert=raid1 -mconvert=raid1` (`cli/src/cmd.rs:655-664`), not the `soft`
  variant `replace`/`remove-missing` use.
- The add path never clears the missing member (`cli/src/add.rs:1476-1497` --
  it balances then clears the journal; no `btrfs device remove`/replace for the
  missing devid). So `missing_count` stays > 0 and `braid status` still reports
  the pool as degraded afterward.

The behavior is intentional (warn, not refuse). The fix is documentation only:
state the operator-visible consequence so nobody is surprised that the pool is
still degraded after a "successful" add.

Scope confirmed with the user: **docs-only**. No code change; the hard-convert-
on-degraded behavior is intentional and out of scope here.

## The change

Single-file, single-bullet edit in `docs/commands/add.md`.

Replace the bullet at line 110:

```
- Warns if the pool has missing devices (suggests `braid replace` first)
```

with:

```
- Warns if the pool has missing devices but does not refuse: `braid add` still attempts to add the new disk. It does not remove or replace the missing member, so even if the add succeeds the pool stays degraded. Run `braid replace` first to repair the missing member and return the pool to full health.
```

Wording rationale:

- "but does not refuse: `braid add` still attempts to add" dissolves the
  refusal misread (the bullet lives under a "refusal cases" heading).
- Deliberately does **not** promise that the RAID1 balance runs: the balance is
  reached only if `btrfs device add` succeeds first. `pool_add_device` returns
  `Err` on a nonzero device-add (`cli/src/pool.rs:235-246`) and the `?` at
  `cli/src/add.rs:1420` propagates it before the `total_after >= 2` balance at
  `:1477`; whether a degraded add succeeds at all is btrfs-dependent. The bullet
  claims only what is airtight -- braid does not refuse, braid never clears the
  missing member, so a successful add still leaves the pool degraded -- tied to
  the observable (`braid status`), not the subtle btrfs data-redundancy question.
- Keeps `braid replace` as the recommended-first action. This matches the
  in-code warning text (`braid replace --missing-id <devid>`,
  `cli/src/add.rs:864-871`) and upstream btrfs guidance
  (`reference/btrfs-progs/Documentation/Balance.rst:168-171`: "use `btrfs
  replace` or `btrfs device remove` to handle the failing/missing device
  first").
- **Does not** mention `braid remove-missing`. On the canonical degraded case
  (2-disk RAID1, one missing) `remove-missing` *refuses* -- the kernel will not
  drop a RAID1 pool below two devices -- so steering operators there to "repair
  first" would be wrong. (The original finding's proposed fix suggested
  `replace`/`remove-missing`; the `remove-missing` half is dropped for this
  reason.)

Use `braid replace` as inline code (no markdown link), consistent with the
other bullets in this section. `replace.md` is already linked from the "Related
commands" section, so no new cross-link is introduced (and no
`mdbook-linkcheck` surface is added).

## Critical files

- `docs/commands/add.md` -- the only file modified (line 110 bullet).

## Out of scope / deliberately not changed

- **Code behavior.** The hard `-dconvert=raid1` balance over a still-degraded
  array (`cli/src/cmd.rs:655-664`) is intentional and stays as-is. The btrfs
  "various problems" warning (`Balance.rst:156-166`) is scoped to converting to
  *lower* redundancy (RAID1->SINGLE), not braid's RAID1->RAID1 convert, so there
  is no correctness hazard to fix here.
- **"What happens under the hood" step 6** (`docs/commands/add.md:89`,
  "If the pool now has 2+ disks: balances data to RAID1"). Left unchanged --
  the safety-checks bullet is the single right home for the degraded caveat;
  duplicating it in step 6 adds maintenance surface for no gain.
- **README.md.** The degraded-add nuance is reference-level detail, not
  cookbook material; the README add example does not need it.

## Verification

1. `Read` `docs/commands/add.md` after the edit to confirm the bullet reads as
   intended and ASCII-only (`--`, straight quotes -- no em-dash/curly quotes).
2. `mdbook build docs` -- must succeed with no `mdbook-linkcheck` errors
   (sanity check that the unchanged cross-links still resolve; no new link was
   added). Per `docs/book.toml` a broken cross-link fails the build.
3. No Rust/VM test requirement -- this is a docs-only edit touching no code
   path. The documented behavior was source-audited (see Context citations),
   not asserted by a dedicated end-to-end test. Existing tests cover only the
   constituent pieces: missing-device counting at the probe layer
   (`probe_pool_degraded_missing_sentinel`, `cli/src/probe.rs:1589-1624`) and
   warning-line routing
   (`tests/cli/braid-add-warnings.py:76-106`, whose own comment says it asserts
   "the warning wiring, not the downstream outcome"). Neither asserts the
   end-to-end claim that a successful degraded add leaves `braid status`
   degraded.
