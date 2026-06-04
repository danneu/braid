# Plan: make unlock.md step 3 state the verify-before-open ordering

## Context

A code-review finding flagged that `docs/commands/unlock.md`'s "What happens
under the hood" list understates the credential-verification ordering that
`braid unlock` guarantees: it verifies the unlock credential against *all*
to-unlock disks and opens *no* LUKS mapper if any disk rejects ("verify all
before open any").

Investigation (this session) established:

- **The invariant is real and load-bearing.** `cli/src/mount.rs#open_disks_with_credential`
  runs one all-or-nothing `verify_credential_for_targets` pass over every
  to-unlock disk and only enters the open loop if it returns `Ok`. The fn doc
  on `cli/src/mount.rs#execute_unlock_and_mount` states the phases
  ("verify credential -> open LUKS"). Pinned by
  `cli/src/mount.rs#non_first_disk_verify_rejection_opens_no_mapper`.
- **Authoritative home is Decision 004**, not 024.
  `docs/design/decisions/004-single-passphrase.md` (single-passphrase): "every
  planned LUKS target is verified before any mapper is opened with that
  credential." Surfaced as Principle 4 (`docs/design/principles.md#4-single-passphrase`).
- **The guarantee is already documented** in unlock.md's Safety section
  (the "fails before opening any mapper and names the failing disk" bullet),
  deliberately aligned in commit `62dea61e`. So it is *not* "omitted."
- **The only genuine residual:** under-the-hood step 3 conveys the ordering
  only implicitly (step 3 precedes step 4). A reader scanning just that list
  can't tell verification is a complete phase before *any* open. The sibling
  `docs/commands/enroll.md` step 5 already inlines this clause ("before any
  keyfile probe"); unlock.md step 3 is inconsistent with it.

This is the minimal fix that removes the ambiguity and makes the two
verify-before-mutate steps consistent -- without duplicating the authoritative
guarantee.

## Change

One edit, `docs/commands/unlock.md`, "What happens under the hood" step 3:

- Before: `Verifies the selected credential against every disk it will unlock`
- After:  `Verifies the selected credential against every disk it will unlock before opening any mapper`

This mirrors the established sibling pattern in `docs/commands/enroll.md` step 5
("Verifies the passphrase against every present pool disk before any keyfile
probe"), making the two steps parallel.

Unchanged on purpose:

- **Step 4** ("Opens LUKS mappers for all locked disks using the verified
  credential").
- **Safety section bullet** -- remains the authoritative user-facing statement
  of the full refusal guarantee (fails before opening any mapper; names the
  failing disk; drift note).
- **Principle 4 / Decision 004** -- already correct.

## Out of scope (deliberately not overboard)

- Restating the full all-or-nothing guarantee / "opens no mapper if any disk
  rejects" / "names the failing disk" in step 3 -- that is the finding's literal
  proposal, and it duplicates the Safety bullet + Decision 004. Rejected.
- ADR cross-links inside the command doc (not house style for `docs/commands/`).
- Touching enroll.md / recover.md / add.md / replace.md / README, or the
  Safety section.

## Why the finding alone was off (for the record)

- Mis-cited the defending test: `passphrase_mismatch_names_failing_disk` pins
  *naming*, not the ordering; `non_first_disk_verify_rejection_opens_no_mapper`
  pins the ordering.
- Mis-cited the ADR: cited 024 (LUKS UUID identity, a separate adjacent Safety
  bullet); correct authority is 004 (single passphrase).
- Claimed the Safety section captures only the "names the failing disk" half;
  it already states both halves ("fails before opening any mapper and names the
  failing disk").

## Verification

- Prose-only change to one Markdown file; no code or tests affected.
- No cross-links added/changed, so `mdbook-linkcheck2` is unaffected; optional
  sanity check: `mdbook build docs` succeeds.
- Read-through: step 3 now reads parallel to `enroll.md` step 5, and the Safety
  bullet is unchanged.
