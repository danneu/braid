# Plan: dedup `docs/commands/recover.md`

## Context

A `/code-review`-style finding (Medium, Clarity) claimed `docs/commands/recover.md`
restates the same gates twice across "What happens under the hood" and "Safety
checks", is the longest command reference, and reads like internals; it proposed
collapsing steps 7-13 into 2-3 sentences and moving the replay mechanics to a
cross-link in `docs/internals/luks-unlock.md`.

Verification confirmed the **core** of the finding (three real intra-page
duplications) but disproved its supporting premises and its prescription:

- **Not the longest page.** recover.md is 1221 words -- 3rd, behind
  `status.md` (2218) and `doctor.md` (1306). It is the longest *mutating-command*
  doc, but `replace.md` (1094) is comparably deep, so depth is house style, not a
  recover anomaly.
- **Wrong relocation target / info-loss risk.** An Explore coverage matrix
  showed the dense mechanics are **unique to recover.md**: per-phase mount-membership
  selection, the full relock cycle (`umount`, `btrfs device scan --forget`, close/reopen
  LUKS, remount), the replay mechanics (`RecoverableBraidLabeled` via `wipefs --all
  --types btrfs` + `btrfs device add -f`; `FreshLuks` replay, `enroll_key_file`
  re-enroll, header backup, no-`-f` add; absent/wrong-label failure), replace/remove-missing
  commit-detection + restore-pre-op logic, and 7 of 9 safety checks. `luks-unlock.md`
  does **not** contain them; `recovery-scenarios.md` covers only the *observable*
  subset at a procedural altitude. Moving them to a bare cross-link would delete
  behavioral contracts that live nowhere else.
- **House style.** Every mutating-command doc uses the "What happens under the
  hood" + "Safety checks" pair and keeps its mechanics inline (`add.md`, `replace.md`,
  `remove-missing.md`). The shared boilerplate refusals (pending-op, pool lock, paused
  balance, UPS `OL`) are intentionally identical across pages.
- **The page intro already states the observable contract** (recover.md opening
  paragraph: "...opening LUKS devices, mounting the pool, rebuilding `pool.json`...
  finishing owed maintenance, and clearing the pending-operation journal"), so no new
  high-altitude summary is needed.

**Intended outcome:** remove the three genuine intra-page duplications by giving
each fact a single owner, while preserving every unique behavioral contract and
respecting the cross-doc/house-style conventions. No relocation, no word-count chasing.

## The three verified duplications (anchor to the fact, not the line)

Line numbers have drifted since the finding was written; in the **current** file
its "line 97" points at the *unique* degraded-mount refusal, which must NOT be
touched. Edits below are keyed to facts.

| # | Fact | Stated in step | Stated again in Safety checks |
| - | ---- | -------------- | ----------------------------- |
| A | `Replace::PoolMutation` on an externally-mounted pool is refused; remediation `braid lock; braid recover` | step 3 (mount step, "Exception:") | the "Refuses to recover `Replace::PoolMutation` when the pool is already mounted..." bullet |
| B | PostAddBalanceRaid1 does no disk prep / btrfs membership mutation, only finishes the owed balance | step 9 | the "Once an add journal reaches `PostAddBalanceRaid1`..." bullet |
| C | replace/remove-missing post-maintenance does not rerun the primary btrfs membership mutation | steps 10-11 | the "Once replace or remove-missing reaches its post-maintenance phase..." bullet |

A 4th, trivial echo (step 1 "Loads pending-op.json (refuses if absent)" vs the
"Refuses if no pending-op.json exists" safety bullet) is left as-is: the parenthetical
aids the step's readability, the bullet matches sibling docs' precondition style, and
the overlap is 4 words. Over-pruning it would hurt readability without material gain.

## Owner principle

- **Standalone gate** (a precondition/refusal whose step content stands without it)
  -> **Safety checks owns it**, trim the step to a brief pointer. Applies to **A**.
- **Refusal intrinsic to a phase's behavioral description** (cannot be removed without
  gutting the step) -> **the step owns it**, drop the redundant Safety-checks bullet.
  Applies to **B** and **C**.

This also *sharpens* "Safety checks" into a list of true refusals an operator hits,
rather than restatements of forward behavior.

## Changes (all within `docs/commands/recover.md`)

1. **A -- keep the Safety-checks bullet, trim step 3.** The Safety-checks bullet
   carries the richer reasoning (admin-mount, kernel-resumed `dev_replace`, "will not
   unmount a mount it does not own") and the remediation, so it is the owner. Rewrite
   step 3 to keep the mount narrative + the *positive complement* (post-maintenance
   recovery on an already-mounted pool is allowed) + a pointer, dropping the duplicated
   reasoning/remediation. Target wording:
   > Opens LUKS devices and mounts the pool (or reuses the existing mount if already
   > mounted). Exception: a `Replace::PoolMutation` journal on an externally-mounted
   > pool is refused (see Safety checks); replace post-maintenance recovery on an
   > already-mounted pool is allowed.

2. **B -- drop the PostAddBalanceRaid1 Safety-checks bullet.** Step 9 already states
   it more specifically ("does not format, enroll, back up headers as target prep, wipe,
   or add disks. It only validates the committed live pool and finishes the owed RAID1
   balance"). Nothing is lost.

3. **C -- drop the post-maintenance no-rerun Safety-checks bullet, and make step 11
   the explicit owner.** Step 10 already states the no-rerun for the not-committed
   PoolMutation branch ("does not rerun `btrfs replace start` or `btrfs device remove`").
   To preserve the *explicit* post-maintenance guarantee the dropped bullet carried,
   append to step 11: "...and finishes only owed maintenance such as resize,
   paused-balance resume, or soft RAID1 balance; it does not rerun the primary btrfs
   membership mutation."

Net: 1 step trimmed (3), 1 step lightly extended (11), 2 Safety-checks bullets removed.
All unique mechanics, the degraded-mount refusal, and every other safety bullet stay.

## What is explicitly NOT done

- No relocation of mechanics to `luks-unlock.md` or any internals page.
- No collapse of steps 7-13; the replay mechanics are unique and stay inline.
- No new high-altitude summary (the intro already provides it).
- No edits to sibling command docs (their cross-section overlap is mild and partly
  intentional boilerplate; recover.md is the only acute case).
- No touching the shared boilerplate refusals or the cross-doc redundancy with
  `recovery-scenarios.md` (different altitude, house pattern).

## Critical file

- `docs/commands/recover.md` -- the only file changed.

## Verification

1. **Build the book / linkcheck:** `mdbook build docs` (cross-links validated by
   `mdbook-linkcheck2` per `docs/book.toml`). The change adds no markdown links --
   "see Safety checks" is intra-page prose, and the existing `recovery-scenarios.md`
   "Related guides" link is untouched -- so linkcheck must stay green.
2. **No-info-loss re-read:** confirm each of B's and C's dropped bullets is fully
   covered by its owning step (B by step 9; C by steps 10-11 after the step-11
   extension), and that A's trimmed step 3 lost only the reasoning/remediation now
   solely in the Safety-checks bullet.
3. **Spot-check the Safety checks list** still contains all true refusals: no-pending-op,
   pool-lock, admission-membership, by-id hard-fail, bootstrap-add, post-mount-probe
   guard, journaled-target-missing, returned-disk-passphrase, degraded exit-2, and the
   Replace external-mount refusal (kept as A's owner).
