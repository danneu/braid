# Delete the btrfs-links audit material

## Context

`btrfs-links/` (319 HTML pages, 34 MB) and `btrfs-links.md` (481-line slug
index) were a one-off research corpus used to mine community btrfs material
for actionable findings against braid. The audit workflow (coordinator +
bucket subagents) ran on 2026-05-23 and produced `btrfs-findings/` (12
local-only markdown files); the actionable conclusion from that audit --
ENOSPC awareness -- has since been implemented (see
`plans/impl/2026-05-23-btrfs-enospc-awareness.md`).

The corpus has served its purpose and is becoming a maintenance liability:
it bloats clones, surfaces in greps, and a live agent prompt currently
tells subagents to consult `./btrfs-links.md`. This plan removes the
corpus and its loose ends from the **current tree** so HEAD no longer
ships it.

**Scope boundary -- history is out of scope.** `git rm` only removes
files from the working tree and index; the blobs remain reachable from
prior commits (verified: `git cat-file -e HEAD:btrfs-links.md` succeeds,
and the corpus was introduced in commit `faf8c13`). If/when the repo is
made public, scrubbing `btrfs-links/` and `btrfs-links.md` from history
is a **separate release gate** with different mechanics (filter-repo /
squash to a fresh root, then force-push to a new public remote) and
different risk (rewrites every downstream clone). That work does not
belong in this commit; it belongs in a release-prep plan run
immediately before publishing.

Historical implementation plans that referenced the audit stay (the
references describe past decisions truthfully), but each gets a one-line
note so a future reader doesn't try to chase the missing paths.

## Inventory of changes

### A. Tracked files to remove (`git rm`)

- `btrfs-links.md` -- the slug index (1 file).
- `btrfs-links/` -- 319 HTML files (`git ls-files btrfs-links/` confirms
  count).

### B. Untracked local files to remove

- `btrfs-findings/` -- 12 markdown files (`b1`..`b10`, `_table.md`,
  `index.md`). Gitignored via a single line in `.git/info/exclude`.
- `prompts/2026-05-24-btrfs-links-planner.md` -- the meta-prompt that
  generated the coordinator prompt; nothing else under `prompts/`
  references it.
- The `btrfs-findings` line in `.git/info/exclude` (line 7) -- now stale.

### C. Live tracked file to edit: `.claude/agents/command-reviewer.md`

Three references to remove or rephrase:

- Line 52-57: "If the command's tools include btrfs, ... consult
  `./btrfs-links.md` selectively and consult `./reference/` first."
  -- drop the `btrfs-links.md` clause; keep the `./reference/` guidance
  (which remains the project's canonical reference source per AGENTS.md).
- Line 61-65: "Do additional web research with `WebSearch`/`WebFetch`
  only when `reference/` and `btrfs-links.md` do not cover what you
  need."
  -- drop the "and `btrfs-links.md`" clause.
- Line 93: bullet listing "References consulted (btrfs-links articles,
  reference/ paths, decision docs)" inside the Findings template
  -- drop "btrfs-links articles, ".

### D. Historical plans -- annotate, do not rewrite

Two tracked plans reference the corpus as background context. Per the
user's choice, keep the bodies intact and add a single bracketed note
where the missing paths first appear, e.g. `(since removed from repo)`.

- `plans/impl/2026-05-23-btrfs-enospc-awareness.md` -- references
  `btrfs-findings/b4-balance-enospc.md` (Context, line 5) and several
  `btrfs-links/*.html` files (line 7-8). Add the "(since removed from
  repo)" note adjacent to the first mention of `btrfs-findings/` and
  the first mention of `btrfs-links/`.
- `plans/impl/2026-05-13-command-reviewer-subagent.md` -- references
  `./btrfs-links.md` twice (lines 165 and 169). Add the note adjacent
  to the first mention; the second is fine without a duplicate.

No other edits to these files -- they are historical records of
finished work.

## Files this plan touches

| Path | Action |
|------|--------|
| `btrfs-links.md` | `git rm` |
| `btrfs-links/` (319 files) | `git rm -r` |
| `btrfs-findings/` (12 files, local) | `rm -r` |
| `prompts/2026-05-24-btrfs-links-planner.md` (untracked) | `rm` |
| `.git/info/exclude` | remove the `btrfs-findings` line |
| `.claude/agents/command-reviewer.md` | edit per section C |
| `plans/impl/2026-05-23-btrfs-enospc-awareness.md` | annotate per section D |
| `plans/impl/2026-05-13-command-reviewer-subagent.md` | annotate per section D |

Nothing else in the repo references these paths. Confirmed by
`git grep -E 'btrfs-link|btrfs-finding'` -- the only tracked matches
are the four files above plus `btrfs-links.md` / `btrfs-links/*` itself.
No `cli/src/`, `modules/`, `docs/`, `justfile`, `scripts/`, `tests/`,
`AGENTS.md`, `CLAUDE.md`, or `README.md` references.

## Pre-conditions (snapshot at run time)

The worktree carries unrelated in-progress edits and untracked files
that vary day to day (e.g. when this plan was drafted, `docs/commands/
recover.md` was modified and several `plans/review/*`, `plans/todo/*`
files were untracked; nothing was staged). Do not encode a fixed list
here -- snapshot live before touching anything, and use the snapshots
as the baseline the verification step compares against:

```
mkdir -p /tmp/btrfs-links-cleanup
git status --porcelain=v1 --untracked-files=all \
  > /tmp/btrfs-links-cleanup/status.before
git diff --name-only --cached \
  | sort -u > /tmp/btrfs-links-cleanup/staged.before
git diff --name-only \
  | sort -u > /tmp/btrfs-links-cleanup/unstaged.before
```

The implementation must not stage, revert, or remove any path outside
the inventory in sections A-D below. In particular, do not touch any
path that appears in `status.before` unless it is in the inventory.

## Verification

After applying the changes and staging the cleanup paths (but **not**
the user's pre-existing edits), the only delta against
`/tmp/btrfs-links-cleanup/staged.before` must be the planned cleanup
paths, and the unstaged delta must be unchanged. Each check below is
scoped so unrelated worktree state cannot cause a false negative.

1. **Tracked corpus is gone.**
   `git ls-files | grep -E '^btrfs-(links|findings)'` returns empty
   (was 320 tracked files).
2. **No live references remain.**
   `git grep -E 'btrfs-link|btrfs-finding'` returns only the two
   annotated `plans/impl/` files. In particular, zero matches in
   `.claude/agents/command-reviewer.md`.
3. **Untracked artifacts are gone.**
   `ls btrfs-links btrfs-links.md btrfs-findings prompts/2026-05-24-btrfs-links-planner.md 2>&1`
   reports all four paths absent.
4. **Local exclude is tidied.**
   `grep -n btrfs-findings .git/info/exclude` returns no match.
5. **Staged-index delta is exactly the planned cleanup paths.**

   Define the planned set:

   ```
   {
     echo btrfs-links.md
     git ls-tree -r --name-only HEAD btrfs-links/
     echo .claude/agents/command-reviewer.md
     echo plans/impl/2026-05-23-btrfs-enospc-awareness.md
     echo plans/impl/2026-05-13-command-reviewer-subagent.md
   } | sort -u > /tmp/btrfs-links-cleanup/planned.set
   ```

   Take the after-snapshots and diff against the before-snapshots:

   ```
   git status --porcelain=v1 --untracked-files=all \
     | sort -u > /tmp/btrfs-links-cleanup/status.after.sorted
   sort -u /tmp/btrfs-links-cleanup/status.before \
     > /tmp/btrfs-links-cleanup/status.before.sorted
   git diff --name-only --cached \
     | sort -u > /tmp/btrfs-links-cleanup/staged.after
   git diff --name-only \
     | sort -u > /tmp/btrfs-links-cleanup/unstaged.after
   ```

   Then assert all of the following are empty:

   ```
   # New staged paths must equal the planned set exactly.
   diff <(comm -23 /tmp/btrfs-links-cleanup/staged.after \
                   /tmp/btrfs-links-cleanup/staged.before) \
        /tmp/btrfs-links-cleanup/planned.set

   # Pre-existing staged paths must still be staged (none unstaged out).
   comm -23 /tmp/btrfs-links-cleanup/staged.before \
            /tmp/btrfs-links-cleanup/staged.after
   ```

   And assert the unstaged set is byte-identical (the user's dirty
   worktree was preserved):

   ```
   diff /tmp/btrfs-links-cleanup/unstaged.before \
        /tmp/btrfs-links-cleanup/unstaged.after
   ```

   And assert that **no unrelated untracked file vanished** -- the
   only `??` line allowed to disappear is the planner prompt
   (`btrfs-findings/` is gitignored and never appears in porcelain
   output, so it does not need to be in this list; step 3 verifies
   its filesystem removal directly):

   ```
   # Lines present in status.before but missing from status.after
   # must be exactly: "?? prompts/2026-05-24-btrfs-links-planner.md".
   # Any other vanished line means an unrelated untracked file was
   # accidentally deleted.
   comm -23 /tmp/btrfs-links-cleanup/status.before.sorted \
            /tmp/btrfs-links-cleanup/status.after.sorted \
     > /tmp/btrfs-links-cleanup/status.vanished
   diff /tmp/btrfs-links-cleanup/status.vanished - <<'EOF'
?? prompts/2026-05-24-btrfs-links-planner.md
EOF
   ```

   This delta-based check works regardless of what the user already
   had staged, modified, or left untracked when the implementer
   started.

6. **Build sanity.** `just test-rust` -- still green. No source code
   was touched, so this is a sanity check only.
7. **Spot-read the modified `command-reviewer.md`.** The "consult
   `./reference/` first" guidance still parses cleanly on its own; the
   findings-template bullet still lists at least two non-empty source
   categories ("reference/ paths, decision docs").

History scrubbing (removing `btrfs-links/` blobs from prior commits) is
**not verified here** -- it is a separate release gate per the Context
section.

## Implementation notes

- Section D annotations use square brackets `[since removed from repo]`
  rather than the parenthetical form in the plan's example. The first
  `btrfs-links/` mention in `plans/impl/2026-05-23-btrfs-enospc-awareness.md`
  sits inside an existing `(cited sources ...)` list, so parentheses
  would nest; square brackets read cleanly there, and using them in
  both annotated files keeps the delimiter uniform.
- In `plans/impl/2026-05-13-command-reviewer-subagent.md` the first
  `btrfs-links.md` mention sits inside a quoted reproduction of the old
  prompt text, so the note is placed just after the closing quote, not
  inside it -- the quoted text stays an accurate record.
- The Section C edits to `.claude/agents/command-reviewer.md` were
  applied with an exact-match script after the interactive editor was
  blocked by the agent-config self-modification guard; the resulting
  text is identical to what the plan specifies.
