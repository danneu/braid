---
name: verify each recipe step against current code before committing to a plan
description: Before drafting a recovery/cleanup-recipe plan, verify every step actually works against the current braid + kernel code. Read the relevant cmd_* / plan_* functions and check assumed flags exist; don't take an issue's plausible-looking recipe at face value.
type: feedback
---

Before writing or committing to a plan that drives a recovery or cleanup
recipe, verify every step in the candidate recipe against the current code.
A plausible-sounding recipe in an issue or doc is a hypothesis, not a
contract — confirm or disconfirm each step before building a test or fix
around it.

**Why:** The #46 investigation (`progress.md`,
`plans/wip/sharded-drifting-beaver-findings.md`) burned multiple plan
drafts on recipes that did not survive contact with the code:
- One draft assumed `braid unlock --allow-degraded` could promote a
  non-degraded mount to degraded. It can't:
  `cli/src/mount.rs:plan_open_pool` only sets `any_missing_member` from
  missing entries in pool.json membership probing, and
  `compile_open_steps` only emits `mount -o degraded` when that flag is
  true. `--allow-degraded` is a *gate* on a degraded plan, not a cause.
- The TL;DR recipe in `progress.md` (`braid lock; braid unlock; braid
  remove-missing disk2`) failed verification because
  `cli/src/remove_missing.rs` checks `pool.missing_count == 0` (kernel
  side) and rejects when the kernel reports a clean topology — even if
  pool.json has a stale entry. There is no current-code braid command
  that reconciles a stale pool.json against a clean live pool. The whole
  recipe direction was based on a misdiagnosis (`recover` is the actual
  fix site), and verifying the recipe up-front would have surfaced this
  weeks earlier.

**How to apply:** For any plan that verifies a recovery, cleanup, or
recipe-style flow:
1. For each step in the candidate recipe, read the relevant `cmd_*`,
   `plan_*`, or kernel/btrfs-progs code and confirm the step actually
   does what the recipe claims. Don't assume a flag exists; grep for it.
2. Check each step's preconditions: what kernel/in-memory state must
   hold for it to succeed, and is that state actually reachable from the
   prior steps?
3. If a step depends on observable state from a kernel async path
   (resume workers, balance workers, scrub workers), confirm whether
   `umount` or other ops actually wait for it. They often don't.
4. If verification turns up that the recipe can't work as written, stop
   and re-anchor the plan on what actually works rather than tweaking
   the recipe. The misdiagnosis is more dangerous than a wrong recipe.
