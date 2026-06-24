# Plan: document the recover read-only refusal in recover.md

## Context

`braid recover` has a tested, fail-closed safety behavior that is missing from
its user-facing docs. When the post-mount probe sees the pool mounted read-only
-- at the VFS layer (mountinfo field 6) or in the filesystem's own options
(field 11) -- recover aborts, leaves `pool.json` unwritten and the pending-op
journal preserved, and prints remediation guidance (`btrfs check`,
`mount -o remount,rw <mount-point>`). The canonical trigger is btrfs auto-remounting the
superblock read-only after an I/O error.

This refusal lives in `cli/src/recover.rs#RecoverCompletion::execute` (the
`mount_check::entry_is_read_only` arm) and is mirrored in the dry-run path of
`plan_recover`. It is pinned by four tests:
`cmd_recover_aborts_when_post_mount_probe_reports_vfs_read_only`,
`..._fs_read_only`, `plan_recover_dry_run_read_only_failure_has_no_foreign_mapper_skip`,
and `plan_recover_dry_run_refuses_already_mounted_read_only_fs_options`.

But `docs/commands/recover.md` "Safety checks" -- a bulleted list that reads as
exhaustive -- has **no** entry for it. The nearest bullet covers a *distinct*
arm of the same probe ("the pool unmounted or with zero btrfs devices"). An
operator who hits the read-only abort has no doc to consult, and the list looks
complete. Outcome wanted: the Safety-checks list matches the four tested
refusal behaviors.

## Why this is a docs-only change (authority already exists)

The design rationale for this refusal is already recorded in the authority tree,
so no ADR or principle edit is warranted -- only the user-facing surface lags:

- **`docs/design/principles.md` (post-commit persist invariant)** already states
  the recover "fail closed with the journal preserved" contract. It enumerates
  the balance-state trigger; it does **not** enumerate the zero-devices,
  unmounted, *or* read-only triggers -- those are instances of the same
  invariant, surfaced in recover.md's Safety checks, not in principles.md.
- **`docs/dev/safety-heuristics.md`** already governs the refuse-vs-warn design:
  "Set fail-closed policy from the downstream failure mode ... every uncertainty
  in that branch is a hard error even if a sibling branch can warn and proceed."
  This is exactly why recover hard-refuses while the mutating commands' preflight
  only warns -- the heuristic is published; nothing to add.
- **`plans/impl/2026-05-11-refuse-recover-read-only-pool.md`** holds the
  implementation history and rejected alternatives (skip-replay; a `PoolState`
  RO field; warn-and-proceed).

braid documents individual recover fail-closed gates via principles.md (the
invariant) + safety-heuristics.md (the heuristic) + per-command Safety-checks
doc sections -- **not** per-gate ADRs. Confirmed: no ADR covers the sibling
zero-devices/unmounted gate. A new ADR would duplicate published authority and
misfit the convention. So the correct, in-convention fix is the one bullet.

## Change

Add **one** bullet to the "Safety checks" section of
[`docs/commands/recover.md`](../../docs/commands/recover.md), adjacent to the
existing post-mount-probe bullet (currently the "...sees the pool unmounted or
with zero btrfs devices..." line). Place the read-only arm **immediately before**
that bullet, so the two arms of the same `RecoverCompletion::execute` probe sit
together and appear in execution order (the read-only check runs first in code,
before the unmounted/zero-devices check).

Proposed text (matches the sibling bullet's voice: same "Refuses to overwrite
`pool.json` or clear `pending-op.json`" opener, same "both preserved -- ...,
then re-run `braid recover`" closer, ASCII `--`, backtick commands; remediation
sourced verbatim from the error string in `recover.rs`):

```markdown
- Refuses to overwrite `pool.json` or clear `pending-op.json` if the post-mount
  probe at the configured mount point sees the pool mounted read-only -- at the
  VFS layer or in the filesystem's own mount options. btrfs may have
  auto-remounted the superblock read-only after an I/O error, or an operator may
  have remounted it; `pool.json` and `pending-op.json` are both preserved --
  investigate with `btrfs check` and remount read-write with
  `mount -o remount,rw <mount-point>`, then re-run `braid recover`.
```

Notes on wording:
- The `<mount-point>` placeholder stands in for the error string's runtime `{mp}`
  substitution (`plan.mount_point` in `RecoverCompletion::execute`). A bare
  `mount -o remount,rw` is an incomplete command, so the doc names the target the
  way the error does. "the configured mount point" in the opener parallels the
  sibling bullet (`recover.md:100`) and tells the reader what `<mount-point>` is.
- Names **both** option layers (VFS field 6 and fs field 11), matching
  `mount_check::entry_is_read_only`, so the doc explains why a btrfs
  auto-remount-ro (which sets only the superblock flag) is still caught.
- Does **not** add a separate `--dry-run` clause. Per ADR 022
  (dry-run-preview-model), preview already mirrors execute's refusals globally;
  calling it out on this one bullet (and no others) would be inconsistent. The
  dry-run mirror remains covered by its existing test.

No change to the numbered "What happens under the hood" list: the post-mount
probe refusals (read-only and zero-devices) live only in Safety checks today, so
adding the read-only arm there keeps the doc's existing structure.

## Out of scope (considered, deliberately excluded)

- **New ADR** -- authority already complete (see above); would duplicate
  principles.md + safety-heuristics.md and break the no-per-gate-ADR convention.
- **`docs/guides/recovery-scenarios.md` walkthrough** -- a read-only-mount
  scenario would be a separate operator-doc addition for a rare case; scope creep
  relative to this finding. Reasonable as an independent follow-up.
- **Mutating-command docs (add/remove/replace)** -- their preflight only *warns*
  on read-only ("proceeding anyway"); recover.md/other docs list *refusals*, not
  warnings, so this is not a parallel gap.
- **README.md** -- contains no per-command Safety-checks content; it defers to
  the mdBook reference. AGENTS.md requires README sync only "when behavior
  changes"; behavior is unchanged here.

## Critical files

- [`docs/commands/recover.md`](../../docs/commands/recover.md) -- the only file
  edited; add the one Safety-checks bullet.

## Verification

1. **No code/test changes.** Behavior is already pinned by the four existing
   tests named in Context; `just test-rust` should remain green (nothing in
   `cli/` changes).
2. **Doc build / link check:** `just docs-build` (runs mdbook +
   `mdbook-linkcheck2`). The bullet adds no new links, so linkcheck is trivially
   satisfied; this confirms the page still builds.
3. **ASCII:** keep the bullet ASCII (`--`, plain quotes) per the repo writing
   convention. (`scripts/docs/check-output-ascii.py` gates `cli/src` and
   `modules` echo lines, not docs markdown, so it will not flag this either way.)
4. **Manual read:** confirm the new bullet sits adjacent to the existing
   post-mount-probe bullet and that the four Safety-checks post-mount/preflight
   refusal behaviors (read-only, unmounted/zero-devices, plus the existing
   pre-flight gates) now all appear in the list.
