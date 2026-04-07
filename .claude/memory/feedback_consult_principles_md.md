---
name: Consult docs/principles.md for braid invariants before proposing behavior changes
description: Before planning changes that touch mount options, balance/replace lifecycle, or any core safety model, read docs/principles.md so you don't propose fixes for states the design explicitly rules out
type: feedback
---

Before planning a behavior change in braid — especially around mount options, balance/replace lifecycle, journal/recovery, or the pool safety model — read `docs/principles.md` and make sure your plan is consistent with its explicit invariants. Do not add code paths that only make sense if an invariant is absent.

**Why:** I proposed a post-mount warning for a *running* btrfs balance after `braid unlock` / `braid recover`. That state is impossible by design because braid always mounts with `skip_balance` (principles.md §3). The existing paused-balance warning exists precisely *because* of that invariant — interrupted balances become paused instead of auto-resumed. I would have caught this by reading principles.md before writing the plan. The same plan also assumed interrupted `btrfs replace` auto-resumes on remount; reference/btrfs-progs/Documentation/btrfs-replace.rst says the opposite on post-v6.19 kernels (replace is canceled on interruption). Both blunders were catchable by reading docs that are already in the repo.

**How to apply:** When a task involves mount options, the unlock/recover/lock lifecycle, balance or replace operations, journal/recovery reconciliation, pool identity/fsid checks, or any property that looks like a safety invariant:

1. Read `docs/principles.md` top-to-bottom before planning. It is short.
2. For btrfs-tool behavior assumptions (auto-resume, cancellation, kernel-version-gated behavior), cross-check `reference/btrfs-progs/Documentation/` on the same pass — not just the source under `reference/btrfs-progs/cmds/`.
3. If your plan contradicts a principle, either (a) drop the plan, or (b) state explicitly that you're proposing to change the principle and justify it. Never silently ignore it.
