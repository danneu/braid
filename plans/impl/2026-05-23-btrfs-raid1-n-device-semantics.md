# Plan: clarify btrfs RAID1 N-device semantics in docs

## Context

A verify-issue investigation surfaced a real, recurring confusion point: btrfs RAID1 always keeps exactly 2 copies of every block regardless of how many devices are in the pool. Adding a 3rd or 4th drive to a braid pool buys more usable capacity, but it does **not** buy 3- or 4-way redundancy. This is famously surprising to NAS users coming from mdadm RAID1 or ZFS mirrors.

The original finding proposed two changes:
1. Modify `braid status` human output to annotate the allocation table profile cell with "RAID1 (2 copies, any N devices)".
2. Add a docs explainer linking to ADR-001.

This plan **pivots away from the status output change** because:
- braid's advisory channel is reserved for actionable state (pending journals, foreign mounts, header backups); always-on educational text would dilute it.
- The `Profile` cell renders whatever `btrfs filesystem df` returns. Mixed-profile cells during partial migration would either render as a comma-joined string or as multiple rows; either way a hardcoded annotation breaks composition.
- `braid status` is run daily and frequently grepped by monitoring; repeating educational prose is noise after the first read.

The remaining gap is documentation. ADR-001 talks about "redundant copy" in the singular but never spells out the 2-copies-regardless-of-N semantic. The "Adding disks over time" section in the day-to-day guide is exactly where an existing-pool operator implicitly asks "what does my next drive buy me?" and currently doesn't answer the redundancy half. And the getting-started guide's `braid add` example uses three drives (`toshiba1`, `toshiba2`, `toshiba3`) -- a first-time 3-drive setup is a first-class scenario where the wrong mental model forms.

Intended outcome: a user who reads any one of ADR-001, the getting-started guide, or the day-to-day usage guide before or during their first 3+ drive operation walks away knowing that extra drives mean more usable space, not extra fault tolerance.

## Scope

Docs only. Three files. No code, no tests, no module changes.

## Changes

### File 1: `docs/design/decisions/001-btrfs-raid1.md`

In the "Tradeoffs accepted" section (currently lines 36-41), insert one new bullet between the existing "50% space overhead" and "No drive independence" bullets so the redundancy point appears next to the related capacity point. Match the file's existing em-dash convention (the file already uses `—`):

> - **Fixed 2-way redundancy** — btrfs RAID1 keeps exactly 2 copies of every block, regardless of pool size. A 3- or 4-drive pool tolerates one drive failure, the same as a 2-drive pool. Additional drives buy usable capacity, not extra fault tolerance. Higher-redundancy profiles (RAID1C3, RAID1C4) exist in btrfs but are not used by braid — the product's redundancy story is "tolerate one drive failure."

### File 2: `docs/guides/getting-started.md`

Between the numbered list ending at line 101 and the "disk names are permanent" paragraph at line 103, insert one short paragraph. Match the file's existing ASCII `--` convention:

> All drives join the same btrfs RAID1 filesystem. btrfs RAID1 keeps exactly 2 copies of every block regardless of how many drives you add, so the pool tolerates a single drive failure -- a 3-drive pool tolerates the same single failure as a 2-drive pool, with more usable capacity. See [Day-to-day usage](day-to-day-nas-usage.md) for what additional drives buy you and how to add them later.

This lands right after the user has seen the 3-drive `braid add` example and the "Create a btrfs RAID1 filesystem across all drives" line item, which is the moment the wrong mental model is most likely to form.

### File 3: `docs/guides/day-to-day-nas-usage.md`

In "Adding disks over time" (currently lines 122-136), after the existing "After adding a disk, existing data gradually rebalances..." sentence, add one short paragraph. Match the file's existing ASCII `--` convention:

> btrfs RAID1 keeps exactly 2 copies of every block no matter how many drives the pool has. A 3rd or 4th drive gives you more usable capacity, but it does not increase fault tolerance -- the pool still tolerates a single drive failure, the same as a 2-drive pool. See [Decision 001](../design/decisions/001-btrfs-raid1.md) for the rationale.

Place this immediately after the existing rebalance sentence so the reader who just learned "add a drive" gets the "and here's exactly what that buys you" follow-up in the same beat.

## Notes on wording choices

- **No per-drive capacity formula.** Avoid claims like "each new drive adds half its raw size" -- true for equal-size drives but wrong for asymmetric pools (e.g. 2x12TB + 1x4TB yields 14TB usable, not 14TB by the half-rule). ADR-001's existing "3x 12TB = ~18TB" example is fine because the drives are equal. The new wording sticks to the load-bearing point ("more capacity, not extra fault tolerance") and lets ADR-001's existing rationale carry the numbers.
- **No "faster rebuilds" claim.** Folklore-grade for btrfs RAID1; not currently supported in ADR-001 or in `reference/btrfs-progs/`. Drop it to avoid drift between the plan and what the project actually claims.
- **No RAID1C3/C4 promotion.** The ADR bullet mentions they exist and are not used; it does not present them as a future option, since the product's redundancy story is fixed.

## Explicitly out of scope

- `cli/src/status.rs` -- no runtime output changes. The original finding's status annotation is dropped entirely.
- `README.md` -- already says "tolerates a single disk failure" (line 37); leave it alone.
- `docs/commands/status.md` -- shows `Profile RAID1` in example output but is not the right place to teach RAID1 semantics.
- `docs/design/principles.md` Principle 6 -- one-line summary linking to ADR-001; the explainer belongs in the ADR it links to, not duplicated here.

## Verification

- `mdbook build docs` from the repo root. Per `AGENTS.md`, this runs `mdbook-linkcheck` and will validate the new cross-link in `day-to-day-nas-usage.md` -> `decisions/001-btrfs-raid1.md` and the new cross-link in `getting-started.md` -> `day-to-day-nas-usage.md`. A broken link fails the build.
- Visual read of the rendered ADR, getting-started, and day-to-day sections to confirm prose flow at the insertion points.
- No tests required (no behavior change, no parser change, no module change). Do not run `cargo fmt` / `just fmt` per repo formatting policy.

## Commit shape

Single commit, conventional commits prefix `docs(raid1):`. Message line 1 (lowercase per `AGENTS.md`): `docs(raid1): clarify 2-copy semantics for N>2 device pools`.
