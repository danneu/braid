# Plan: document lock's "verify-or-leave-open" cleanup case in lock.md

## Context

A review finding (Low / project-fit) flagged that `docs/commands/lock.md`
describes the lock cleanup flow -- "What happens under the hood" steps 4-6
(forget, then close member-owned, then close orphan `braid-*` mappers) -- as if
every `braid-*` mapper is closed, but never mentions the deliberate third
outcome mandated by ADR-024 rule 6: a scanned `braid-*` mapper whose backing
LUKS UUID cannot be verified is reported with a `[warn]` and **left open**
(excluded from both forget and close), leaving cleanup incomplete. An operator
reading only the cookbook would expect every mapper closed and be surprised by a
survivor.

The original finding also named a second trigger -- a `null_underlying` mapper
whose persisted devid is corrupt/ambiguous (ADR-024 rule 7). Review showed that
path is **not reachable through normal `braid lock` dispatch** (see "Out of
scope" below), so it is deliberately left out of this docs change; the bullet is
scoped to the unverifiable-backing-LUKS-UUID case only.

Verification against `cli/src/lock.rs` confirmed the behavior and corrected one
imprecision in the finding's proposed wording: the literal `cleanup incomplete`
summary line is emitted **only on the `--dry-run` preview path**
(`LockPlan::preview`), never on a real run. A real `sudo braid lock` surfaces
the per-mapper `[warn]` (`skipped_mapper_warn_body`) and, when cleanup is
uncertain, suppresses the `pool already locked` line -- yet still exits 0.

Outcome: the reference page should document this third outcome accurately, so an
operator who sees a surviving `braid-*` mapper after `braid lock` understands it
was intentional and knows the remediation.

## Decision

Add **one bullet to the existing "## Error handling" section** of
`docs/commands/lock.md`. Do **not** edit steps 4-6 -- they are already accurate
("the planned close-set mappers" excludes skipped mappers by construction); the
only gap is that the skipped category is never named.

Rationale (braid-specific):

- **House style.** lock.md already keeps happy-path in "What happens under the
  hood" and deviations in "Error handling" -- step 4 says forget runs; an
  Error-handling bullet says forget is skipped when unmount fails. The
  verify-or-leave-open case is the same shape (a deviation), so it belongs in
  Error handling, mirroring the existing split.
- **Single authoritative home.** braid's docs hygiene (single source of truth /
  docs-consolidation) discourages restating one fact in two sections, so not
  "both" and no duplicate parenthetical on step 6.
- **ADR cross-link is conventional.** `docs/commands/*.md` already links ADRs
  (`doctor.md` -> ADR-024, `add.md` -> ADR-027, `idle.md` -> ADR-016). ADR-024
  (status: Active) rule 6 is the live authority for this rule, so link it.

## Change

File: `docs/commands/lock.md` -- append as the **final** bullet of
"## Error handling" (keeps the existing unmount-failure cluster -- the
skip-forget, device-busy-downgrade, and lsof/fuser bullets -- intact):

> - If a `braid-*` mapper's backing LUKS UUID cannot be verified (for example its
>   backing device is gone or its LUKS header is unreadable), lock prints a
>   `[warn]`, leaves that mapper open instead of closing it (it is excluded from
>   both `btrfs device scan --forget` and the close step), and still exits
>   cleanly. Cleanup is incomplete: investigate the mapper, then re-run
>   `braid lock` once its LUKS UUID is readable. The literal `cleanup incomplete`
>   summary line appears only under `--dry-run`; a real run surfaces the
>   per-mapper `[warn]` and does not print `pool already locked`. See
>   [ADR-024](../design/decisions/024-luks-uuid-identity.md#runtime-handles-and-labels).

Required content (all must survive any wording tweak):

1. Trigger: a scanned `braid-*` mapper whose backing LUKS UUID cannot be
   verified.
2. Outcome: `[warn]`, mapper left open, excluded from forget + close, exits
   cleanly.
3. Remediation: re-run `braid lock` once the mapper's LUKS UUID is readable.
4. Precision correction: `cleanup incomplete` is the `--dry-run` signal; a real
   run shows the per-mapper `[warn]` (this is the substance of the fix -- keep
   it even if trimming for length).
5. ADR-024 cross-link with the `#runtime-handles-and-labels` anchor.

Style: ASCII `--` (not em-dash), backticks for literal output, real Markdown
link (linkcheck-validated).

## Out of scope (decided)

- **Duplicate-devid `null_underlying` skip is deliberately omitted** (reviewer
  finding). It is unreachable through normal `braid lock` dispatch:
  `load_membership_from` (`cli/src/membership.rs#load_membership_from`) rejects a
  pool.json with one devid shared by two members as `MembershipError::Conflict`,
  and all three lock entry points (dry-run, real, systemd-stop) load via
  `load_membership_for_lock` (`cli/src/main.rs#load_membership_for_lock`), which
  falls back to `PoolMembership::empty()` on that error. With empty membership,
  Pass 2's `by_devid` returns `Ok(None)` (orphan close), never `DuplicateDevid`;
  the lock.rs Pass 2 `DuplicateDevid` skip (`build_close_sets_full`) is exercised
  only by `membership.rs` tests that build corrupted membership directly.
  Documenting it for operators would need a separate implementation plan that
  makes the path observable through real dispatch.
- **No code change.** `cli/src/lock.rs` behavior is correct and matches ADR-024.
- **No README change.** README is the brief cookbook and has no lock
  under-the-hood content; this prior-crash edge case belongs in the mdBook
  reference tier.
- **No sibling command-doc change.** No other `docs/commands/*.md` page
  describes this flow.
- **No ADR/internals edit.** ADR-024 already states the rule; this is the
  user-facing tier catching up.

## Adjacent gap noticed (not in this plan)

Verifying the duplicate-devid finding surfaced a separate, genuinely reachable
behavior that lock.md also does not document: when pool.json is missing,
unreadable, corrupt, or conflicting, `braid lock` does not refuse -- it warns and
proceeds with **empty membership** (`load_membership_for_lock`), so every
observed `braid-*` mapper is verified by LUKS UUID and closed as an orphan. This
is the real production behavior for a corrupt/duplicate-devid pool.json. It is a
legitimate candidate for its own Error-handling bullet but is out of scope here:
the original finding is only about the unverifiable-mapper survivor, and this
plan stays a one-bullet docs fix. Flagging it so a follow-up can pick it up.

## Critical files

- `docs/commands/lock.md` -- the only edit (append one Error-handling bullet).
- `docs/design/decisions/024-luks-uuid-identity.md` -- link target (read-only);
  rules 6-7 live under "## Runtime Handles And Labels" (slug
  `runtime-handles-and-labels`).
- `cli/src/lock.rs` -- behavior source (read-only) for reviewer verification:
  `LockPlan::preview` (dry-run-only `cleanup incomplete`), `LockPlan::execute`
  (real-run warns + `pool already locked` suppression at the
  `!cleanup_uncertain` gate), `skipped_mapper_warn_body`,
  `push_uuid_classified_candidate` (the `Err` arm that skips an unverifiable
  candidate and marks cleanup uncertain).
- `cli/src/membership.rs#load_membership_from` and
  `cli/src/main.rs#load_membership_for_lock` -- read-only; ground the
  duplicate-devid scoping decision in "Out of scope" above.

## Verification

- `mdbook build docs` -- builds the book and runs `mdbook-linkcheck2`,
  validating the new `024-luks-uuid-identity.md#runtime-handles-and-labels`
  cross-link and anchor (a wrong slug fails CI).
- Eyeball the rendered "Error handling" section to confirm the new bullet reads
  cleanly alongside its siblings.
- No Rust/VM tests: docs-only change, no behavior touched.
