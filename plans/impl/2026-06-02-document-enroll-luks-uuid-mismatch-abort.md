# Plan: document the LUKS UUID-mismatch abort on `braid enroll`

## Context

`braid enroll` aborts with a hard error when a present pool disk's live LUKS
UUID does not match the UUID recorded in `pool.json` (a swapped, cloned, or
reformatted disk). The check lives in `discover_enrollment_candidates`
(`cli/src/enroll_key_file.rs#discover_enrollment_candidates`, the
`ConfigDiskState::PresentLuks` arm) and emits the shared remediation text from
`cli/src/luks.rs#luks_uuid_mismatch_guidance`. It enforces the decision-024
"LUKS UUID is disk identity" invariant at the enroll mutation boundary, and is
already covered by tests (`tests/cli/enroll-uuid-mismatch.py` and the
`discover_rejects_luks_uuid_mismatch_before_slot_inventory` unit test).

The behavior is real but **undocumented**: `docs/commands/enroll.md` step 4
describes discovery as only skipping absent/non-LUKS disks, which implies any
present LUKS disk simply gets enrolled. The commit that added the check
(`af7d1406`) touched no docs, so the page silently lagged the code. The other
pages that document this same member-UUID-mismatch (swap/clone/reformat)
boundary -- `status.md` (the **LUKS UUID MISMATCH** disk-states row),
`doctor.md` (`declared_disks` Fail), and `recovery-scenarios.md` ("Out-of-band
reformat during recovery") -- all surface it; enroll is the lone mutation-path
page that omits it. (`unlock.md` only mentions the match check in passing, and
`replace.md`/`discover.md` document *adjacent* UUID boundaries -- new-disk
collision and duplicate/cloned-UUID among scanned disks -- not a present
member's UUID contradicting its `pool.json` record.) The outcome: a reader
can't anticipate the abort, and the page contradicts the architecture authority
by omission.

This is a docs-only change. No code or test changes.

## Scope

- **Edit:** `docs/commands/enroll.md` only.
- **Do not touch:** `README.md` (enroll is a one-line table entry by design --
  cookbook stays brief), `docs/guides/auto-unlock.md` (prescriptive, no
  discovery walkthrough), tests (behavior already covered), or code.
- **No ADR link:** match enroll.md's existing linkless prose style; command
  pages *link* to decision-024 exactly once (doctor.md) and only where design
  rationale is needed -- `status.md` mentions "decision 024" in prose but adds
  no link. Plain prose avoids needless mdbook-linkcheck surface.

## Edits to `docs/commands/enroll.md`

The page intentionally double-lists its preconditions (the journal refusal and
the `--generate` mount-point check each appear in both the numbered "What
happens under the hood" list and the "Safety checks" list). Follow that
established pattern: add the UUID-mismatch abort to **both** sections, framing
step 4 as flow ("aborts before...") and the safety bullet as contract
("Refuses if...").

Wording mirrors `luks_uuid_mismatch_guidance()` verbatim in intent -- the
"swapped, cloned, or reformatted" triad (as in `status.md`) and **both**
remediation paths (detach/reattach the original *or* `braid replace` if
intentional). Do not ship the narrower "resolve with `braid replace`" framing:
`braid replace` is correct only for an intentional swap; an accidental
wrong-disk is fixed by reattaching the original, no replace needed. Use ` -- `
(double hyphen, the file's existing style), not an em-dash.

### 1. Step 4 (numbered list, "What happens under the hood")

Current:

> 4. Scans pool membership for present LUKS disks. Absent or non-LUKS disks are skipped with a message.

Replace with:

> 4. Scans pool membership for present LUKS disks. Absent or non-LUKS disks are skipped with a message. If a present disk's live LUKS UUID does not match the UUID recorded in `pool.json` -- the disk was swapped, cloned, or reformatted -- enrollment aborts before any passphrase prompt or slot change; detach the foreign disk and reattach the original, or run `braid replace` if the swap was intentional.

Rationale for "before any passphrase prompt or slot change": discovery
(`plan_enroll` -> `discover_enrollment_candidates`) runs before
`EnrollPlan::execute` reads the passphrase, so the abort leaves nothing mutated
and no secret entered. This holds for `--dry-run` too (discovery runs
unconditionally before the `params.dry_run` guard), so the existing dry-run
example needs no separate note.

### 2. Safety checks (bullet list)

Add a new bullet, placed right after the pool-lock bullet (grouping the
early/identity preconditions before the `--generate`-specific bullets):

> - Refuses if a present disk's live LUKS UUID no longer matches its `pool.json` record -- the disk was swapped, cloned, or reformatted; detach the foreign disk and reattach the original, or run `braid replace` if the swap was intentional.

## Verification

1. `mdbook build docs` -- must pass (no new links, but the build + linkcheck
   must stay green).
2. Visually render `docs/commands/enroll.md`; confirm step 4 reads cleanly and
   the new Safety-checks bullet sits after the pool-lock bullet.
3. Cross-check the doc's wording against its true sources.
   `cli/src/luks.rs#luks_uuid_mismatch_guidance` is the *sole* source-of-truth
   for the remediation paths (detach/reattach the original, or `braid replace`
   if intentional) -- the doc reproduces what the CLI prints. The "swapped,
   cloned, or reformatted" triad also matches the **LUKS UUID MISMATCH** row of
   the disk-states table in `status.md`, but cite that row for the triad only:
   its own remediation is "run `braid doctor`", not enroll's. Both must match so
   the page can't drift from the actual error text.
4. Confirm no other file changed (`git status` shows only
   `docs/commands/enroll.md`).
