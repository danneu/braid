# Plan: docs cross-link from header-mutating command pages to off-system header-backup posture

## Context

A code-review finding flagged `docs/commands/enroll.md` for two gaps:

1. It allegedly never mentions that `braid enroll` takes the pool lock and
   fails fast on contention.
2. Step 10 ("Creates a LUKS header backup for each modified disk") never
   tells the reader that the backup lands locally and must be exported
   off-system and deleted.

Investigation (see verify-issue output earlier this turn) confirmed:

- Claim (1) is **stale**. `enroll.md:73` already reads
  "Refuses if another braid operation is in progress (pool lock
  `/run/braid-pool.lock` is held) -- retry once it finishes." That bullet
  was added in commit `3a41f06 docs(commands): document fail-fast
  pool-lock contention` and matches the wording in every other
  mutating-command doc.
- Claim (2) has a real kernel, but it is **not enroll-only**.
  - `add.md:86`, `replace.md:86`, and `recover.md:75` all mention a LUKS
    header backup without any off-system pointer. `recover.md:75`
    specifically says "...the keyfile is re-enrolled, then the LUKS
    header is backed up..." as part of replaying a journaled add target.
    `cli/src/recover.rs:800,846` is the code path.
  - The authoritative off-system posture lives at `docs/commands/status.md:210-225`
    (warning text) and `docs/internals/luks-unlock.md:127-145` (rationale
    + messaging invariant).
  - The messaging invariant in `luks-unlock.md:137-145` restricts
    recovery / restoration wording from pointing at the local file as
    the *backup source* (e.g. "restore from `/var/lib/braid/luks-headers/...`").
    A forward pointer that says "you just produced a fresh local
    backup, copy it off-system and delete it" pushes readers AWAY from
    depending on the local file -- the opposite direction the
    invariant warns against -- and is permitted in any "under the hood"
    or step-description context, including `recover.md`.
  - The reviewer's first round caught a separate problem with the
    target itself: at `status.md:210` the "Pending LUKS header
    backups" surface is **bold paragraph text**, not a Markdown
    heading, so there is no `#pending-luks-header-backups` anchor for
    the cross-link to land on. Status.md must be edited to expose a
    real heading.

The ideal pivot is therefore: drop the pool-lock half of the finding
entirely (no change needed), promote the existing bold paragraph in
`status.md` to a real heading so an anchor exists, then apply a single
consistent one-line cross-link to every command doc whose flow
produces a local LUKS header backup -- enroll, add, replace, **and**
recover. Sibling-consistent across all four producing-command docs is
the goal -- adding the pointer to a subset would create exactly the
kind of docs-drift the original finding complains about.

## Scope

Five files change. Four are command docs that each gain the same
one-line cross-link next to their header-backup mention; one is the
target page that needs a real heading at the link's landing site.

- `docs/commands/status.md` -- promote `**Pending LUKS header
  backups.**` (currently bold paragraph text at line 210) to a real
  Markdown heading `#### Pending LUKS header backups`. The
  surrounding section is `### Advisories` at line 168, and every
  advisory in that section is a peer bold-paragraph; promoting this
  one advisory to a level-4 heading nests it correctly under
  Advisories. (Level-4, not level-3 -- a level-3 heading would make
  it a sibling of Advisories instead of a child.) This creates the
  `#pending-luks-header-backups` anchor that the new cross-links
  target. Make no other content edits in `status.md`; leave the
  sibling bold-paragraph advisories (`**Foreign filesystem at the
  mount point.**`, `**Pending recovery journal.**`, `**ENOSPC risk on
  RAID1 pool.**`, etc.) untouched.
- `docs/commands/enroll.md` -- one-line cross-link after step 10 of
  "What happens under the hood" (line 68).
- `docs/commands/add.md` -- one-line cross-link as a follow-on
  sentence after step 3 of "What happens under the hood" (line 86),
  where "creates a LUKS header backup" sits mid-sentence.
- `docs/commands/replace.md` -- restructure so the cross-link
  applies to both producing paths. The current step 3 only describes
  the fresh-disk header backup. The existing-LUKS-with-`--enroll`
  path -- exercised by `tests/cli/replace-enroll-existing-luks.py`
  and implemented at `cli/src/replace.rs:706-730` -- also produces a
  local header backup, but **only when `--enroll` actually mutates
  slot 1**: the planner at `cli/src/replace.rs:1397-1418` resolves
  the user's `--enroll` flag to `None` if the keyfile is already
  enrolled (`DiskEnrollAction::AlreadyEnrolled`), and the apply path
  gates both the enrollment and the post-mutation backup behind that
  resolved `Some(kf)` at `cli/src/replace.rs:706,724`. Add a single
  paragraph immediately below the numbered list (before "A sleep
  inhibitor is held...", around line 98) that says the fresh-disk
  path always produces a local LUKS header backup (step 3), and the
  existing-LUKS path produces one **when `--enroll` actually adds
  slot 1** (already-enrolled disks are no-ops -- no enrollment, no
  backup), then the cross-link.
- `docs/commands/recover.md` -- one-line cross-link after step 8 of
  "What happens under the hood" (line 75), which is where "...then
  the LUKS header is backed up..." sits mid-sentence as part of
  replaying a journaled fresh-LUKS add target.

No other files change. `docs/internals/luks-unlock.md` and the
existing prose body under `status.md`'s warnings section remain the
canonical guidance.

## Edit pattern

Cross-link line, identical in every command doc:

```
See [Pending LUKS header backups](status.md#pending-luks-header-backups) -- copy each `.luksheader` off-system and delete the local copy.
```

(Anchor verification: heading text "Pending LUKS header backups"
generates the mdbook slug `pending-luks-header-backups`. Confirm at
edit time by running `mdbook build docs` and checking that no
linkcheck warning fires; if mdbook generates a different slug for any
reason -- a duplicate-heading suffix, for instance -- use the slug it
emits.)

Placement per file:

- `enroll.md` step 10 -- new line directly under "10. Creates a LUKS
  header backup for each modified disk." as a follow-on paragraph or
  sub-bullet.
- `add.md` step 3 -- the header-backup mention is mid-sentence inside
  step 3 ("...creates a LUKS header backup, and opens the LUKS
  mapper"), so the cross-link goes as a separate follow-on sentence
  *after* the numbered step, not inline.
- `replace.md` -- add one paragraph immediately after step 10 / before
  the "sleep inhibitor" paragraph (line 98). Two sentences: first
  states that the fresh-disk path (step 3) always produces a local
  LUKS header backup, and the existing-LUKS path produces one **only
  when `--enroll` actually adds slot 1** (an `--enroll` against a
  disk where the keyfile is already enrolled is a no-op and produces
  no backup -- see `cli/src/replace.rs:1397-1418`). Second sentence
  is the standard cross-link.
- `recover.md` step 8 -- new line directly under step 8 as a follow-on
  paragraph or sub-bullet; the header-backup clause is mid-sentence
  inside step 8, so the cross-link goes as a separate sentence after
  the numbered step.

`status.md` heading promotion is a one-line edit: change

```
**Pending LUKS header backups.** When a header-mutating operation
```

to

```
#### Pending LUKS header backups

When a header-mutating operation
```

(Level 4 because the containing section is `### Advisories` at
`status.md:168`. Do not touch any sibling bold-paragraph advisory.)
The body prose ("When a header-mutating operation...") and the
existing nested cross-link to
`docs/internals/luks-unlock.md#header-backup-workflow-and-messaging`
stay byte-for-byte the same.

## Verification

- `mdbook build docs` succeeds with no `mdbook-linkcheck` warnings or
  failures. Broken cross-links fail CI per `docs/book.toml` (see
  `AGENTS.md` Documentation section), so a clean build proves every
  new `#pending-luks-header-backups` anchor resolves.
- Manual: render the four command pages in the built mdbook output
  and click the new link from each -- it must jump to the "Pending
  LUKS header backups" section in `status.md`, not 404 and not land
  at the top of the page.
- `grep -nE "header backup|luksheader|pending-luks-header-backups"
  docs/commands/{enroll,add,replace,recover,status}.md` shows the new
  cross-link on the four command pages and the promoted heading in
  `status.md`.
- `git status --short` lists exactly five docs files: the four
  command pages plus `status.md`. Nothing under `cli/src/`,
  `tests/`, `modules/`, or any other path.
- No test changes needed -- this is a docs-only edit. The advisory's
  underlying code paths are untouched, so existing coverage still
  applies:
  - Advisory scan implementation: `cli/src/luks.rs:1104-1125`
    (`header_backup_advisories_in` + the `header_backup_advisories`
    `StatePaths` wrapper).
  - Advisory unit tests: `cli/src/luks.rs:2739-2823`
    (`advisory_empty_when_dir_missing`, `advisory_empty_when_dir_empty`,
    `advisory_present_for_luksheader`, `advisory_ignores_legacy_img_files`,
    `advisory_ignores_unrelated_files`, `advisory_via_state_paths`).
  - TUI render: `cli/src/tui/view/mod.rs:1974-1987`
    (`snapshot_with_advisory`).
  - VM tests that exercise the backup-producing flows (not the
    warning surface itself) -- e.g.
    `tests/module/pool-lock-enroll-contention.py` for enroll's
    lock contract, `tests/cli/replace-enroll-existing-luks.py` for
    the existing-LUKS-with-`--enroll` replace path -- continue to
    pass without changes.

## Non-goals

- Do not touch the pool-lock bullet at `enroll.md:73` (or its sibling
  bullets in add/replace/recover) -- already present, original
  finding was stale.
- Do not move the canonical off-system guidance out of `status.md`
  or `docs/internals/luks-unlock.md`. The pointer is one-way: command
  docs -> status.md -> internals.
- Do not edit any prose in `status.md` other than the single
  bold-paragraph-to-heading promotion. The warning text, code-block
  example, and existing internals cross-link stay verbatim.
- Do not add new wording to any "Safety checks" section; the
  header-backup posture is an outcome of the operation, not a refusal
  case.
- Do not introduce a new internals-doc anchor or duplicate the
  off-system rationale into the command docs. Command docs get one
  cross-link sentence each; readers drill into `status.md` for the
  warning, and from there into `luks-unlock.md` for the full
  rationale.
