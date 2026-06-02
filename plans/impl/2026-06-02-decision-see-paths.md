# Plan: clean dead `## See` pointers + codify and enforce the decision-doc reference convention

## Context

A review finding flagged that `docs/design/decisions/002-config-first-workflow.md`
points, in its `## See` section, to `scripts/braid-add-disk.sh` -- a script that
was deleted long ago (`ee12ad34 "kill old braid-* scripts"`, 2026-02-21). The
finding is right that the pointer is dead and that ADRs 007 and 008 carry the
same dead bullet.

But the finding's proposed fix -- repoint the `## See` bullets to the live
successor code (`cli/src/membership.rs`, `cli/src/add.rs`) -- is the wrong shape.
All three ADRs are **Superseded**: their bodies describe a dead
`init-disk`/`apply`/`braid-add-disk` world throughout. Repointing one `## See`
bullet to live unified-CLI code, while the body still narrates the old two-script
workflow, manufactures internal inconsistency in a frozen historical record. The
supersession banner (`> Superseded by ...`) is already the forward pointer.

Investigation also surfaced two things the finding missed:

1. **A fourth instance of the same class, in an Active ADR.**
   `docs/design/decisions/004-single-passphrase.md:48` points to
   `design-docs/1-braid-add-disk.md`, a design doc deleted in
   `126438a7 "delete dead docs"` (2026-04-08). Because 004 is `status: Active`,
   this is the higher-priority dead pointer.

2. **The repo already has the correct pattern for removed-but-valuable refs.**
   `002:65` and `003:53` reference removed archive files using a deliberate
   git-history-note form:
   `(preserved in git history; last present at commit 9df91f9)`. These are NOT
   bugs -- they are the gold standard. They must be left untouched.

Intended outcome: every ADR `## See` bullet either resolves to a live path or
carries an explicit git-history note. Recurrence is prevented on two fronts: a
prose convention in `AGENTS.md` (for the judgment-call half a machine cannot
check -- "don't repoint a frozen doc at current code"), and a mechanical
`scripts/docs/check-see-paths.py` lane (for the detection half -- "a `## See`
path no longer resolves") wired into an *always-on* CI job that is not
docs-path-filtered, so a referenced-file deletion anywhere in the tree (the
`scripts/` deletion class that broke this originally) fails CI at introduction.
The original pointer survived ~3.5 months precisely because code-span paths dodge
`mdbook-linkcheck2` (the `File
References` section says so: "do not linkify code paths ... dodges linkcheck");
the new check closes that gap and is the regression test the deletions would
otherwise lack.

## Decision

Two decisions, resolved for braid's character (consistency-first, drift-proof
references, conventions centralized in `AGENTS.md`):

- **Dead refs:** drop the deleted-*script* bullets; git-note the deleted
  *design-doc* bullet. The git-note form exists to preserve archived *analysis*
  with lasting value (002:65 "original best practices analysis", 003:53 "original
  plan and research"); a deleted error-stub script has no such value, so it is
  dropped, not noted. 004:48 is an archived design doc -- the same category as its
  siblings -- so it is git-noted to match them.
- **Recurrence prevention (two parts):**
  - *Prose* in `AGENTS.md`, next to the existing `File References` section --
    carries the judgment call no check can encode (don't repoint a frozen
    superseded doc at current code). `AGENTS.md` is what agents/automated
    reviewers read, so it is the home that actually stops the *wrong fix*.
  - *A mechanical check* (`scripts/docs/check-see-paths.py`, in a new always-on
    `checks.yml` CI lane -- not docs-path-filtered, so it fires on the `scripts/`
    deletion class that broke this) that asserts every `## See` target path
    resolves -- stops the *undetected pointer*. Prose alone could not: the
    periodic human audit that prose relies on is exactly what failed here.

## Changes

### 1. Drop the dead script bullets (3 superseded ADRs)

Delete the entire `scripts/braid-add-disk.sh` bullet line from each `## See`
section. Do not repoint, do not git-note (the script is deleted dead code).

| File | Line | Exact bullet to delete |
| --- | --- | --- |
| `docs/design/decisions/002-config-first-workflow.md` | 64 | `` - `scripts/braid-add-disk.sh` — error stub directing to `init-disk` + `apply` `` |
| `docs/design/decisions/007-disk-pool-management.md` | 117 | `` - `scripts/braid-add-disk.sh` — existing add script with auto-evict `` |
| `docs/design/decisions/008-unified-cli.md` | 92 | `` - `scripts/braid-add-disk.sh` — error stub directing to `init-disk` + `apply` `` |

### 2. Git-note the dead design-doc bullet (1 active ADR)

In `docs/design/decisions/004-single-passphrase.md:48`, convert to the
git-history-note form already used in 002:65 / 003:53. Keep the em-dash
separator (the surrounding file uses `—`, not `--`).

- From: `` - `design-docs/1-braid-add-disk.md` — original script design (historical) ``
- To:   `` - `design-docs/1-braid-add-disk.md` — original script design (preserved in git history; last present at commit `4112e57`) ``

The hash `4112e57` is the parent of the deletion commit `126438a7`; the file is
confirmed present there (`git cat-file -e 4112e57:design-docs/1-braid-add-disk.md`).
Use the 7-char form to match the gold-standard notes (002:65 / 003:53 use 7-char
`9df91f9`); both 7-char prefixes are confirmed unambiguous via `git rev-parse`
(`git rev-parse --short` pads to 8, but a hand-written unambiguous 7-char prefix
resolves and matches the existing style).

### 3. Codify the convention in `AGENTS.md`

Insert a new subsection immediately after the `File References` section (after
the `plans/wip` exemption paragraph at AGENTS.md:196, before `## Git Commits`).
Use ASCII `--` to match `AGENTS.md` style. Draft:

```markdown
## Decision Doc References

A decision doc with `status: Superseded` or `Deprecated` is a point-in-time
record. Do not rewrite its body or `## See` section to track current code -- the
`> Superseded by ...` banner is the forward pointer to live artifacts. Repointing
a frozen doc's references at today's successor code only makes it contradict its
own narrative.

Independent of status, a `## See` bullet whose path no longer resolves is a broken
pointer, not history. Drop it; or, if the removed file has lasting reference value
(an archived design doc or plan -- not deleted dead code), replace the bare path
with the git-history-note form used in 002 and 003:
`(preserved in git history; last present at commit <hash>)`. The `## See` path
half of this rule is enforced by `scripts/docs/check-see-paths.py`.
```

(Heading is status-neutral on purpose: the second paragraph's dead-pointer rule
governs Active ADRs too -- it is the rule behind this plan's own 004 fix -- so it
must not be filed under a "Superseded"-only label.)

### 4. Add a mechanical `## See` path-resolution check

New `scripts/docs/check-see-paths.py`, following the pattern of the sibling
`scripts/docs/check-*.py` scripts (focused, one concern; exit non-zero with a
`path:line` failure list, else print ok). Behavior:

- For each `docs/design/decisions/*.md`, locate the `## See` section (bullets
  from the `## See` heading to the next `## ` heading or EOF).
- For each bullet, validate **every code span in the target cluster** -- the
  backtick spans before the first description separator (` — ` em-dash or ` -- `;
  the whole bullet if it has no separator, e.g. the bare-path bullets in ADR 024).
  This both covers multi-path bullets (ADR 001:59 lists two test paths before the
  dash) and is required for correctness: code spans *after* the separator are
  description, not paths -- option names (`braid.disks`), unit names
  (`braid-online.service`, `braid-pool.target`), and symbols (`DiskMember`) that
  would false-positive if resolved as files. Markdown-link bullets
  (`- [text](...)`) have no leading code span and are skipped (mdbook-linkcheck2
  validates those).
- Strip a trailing `#anchor` and a trailing `:NN` / `:NN-MM` line suffix before
  resolving. Skip any bullet whose line contains `preserved in git history` (the
  git-note form deliberately points at removed files).
- Assert the resulting path exists on disk (a bare directory like `cli/src/`
  counts as existing).

Wiring -- this check must run **always-on**, not docs-path-filtered. The original
bug was a `scripts/` deletion (`ee12ad34`) orphaning a docs ref; `docs.yml` is
filtered to `paths: [docs/**, .github/workflows/docs.yml, flake.nix, flake.lock]`
and `test.yml` is toggled off (`workflow_dispatch` only), so a `scripts/` (or any
non-docs) change is the exact event class no current lane gates. Wiring the check
into `docs.yml` would only shorten the latency, not close the gap the plan exists
to close.
- New `.github/workflows/checks.yml`: an always-on lane (`on: push` to `master` +
  `pull_request`, with **no `paths:` filter**) whose single job is
  `actions/checkout@v4` then `run: python3 scripts/docs/check-see-paths.py`. The
  script is stdlib-only (`re`/`sys`/`pathlib`, like its siblings), so it runs
  *without* `nix develop` -- a sub-second step on `ubuntu-latest`'s preinstalled
  `python3` that does not reintroduce the cost that disabled `test.yml`.
- `justfile`: add a `check-docs-see-paths` recipe running
  `python3 scripts/docs/check-see-paths.py` (parallel to `check-code-doc-anchors`,
  justfile:258-260) for local use. CI calls the script directly rather than via
  `just`/`nix` to keep the always-on lane dependency-free.

Scope notes:
- Validates *path resolution* only. It deliberately does not flag line-number
  suffixes (it strips `:NN-MM` and resolves), so ADR 021's `cli/src/unlock.rs:93-96`
  (a live path) is not failed by it -- see non-goals. A line-number-suffix check is
  a possible future extension, out of scope here.
- The sibling docs checks (`check-frontmatter`, `check-doc-tables`,
  `check-code-doc-anchors`) stay in the docs-path-filtered `docs.yml`: they
  validate docs-internal consistency, which only changes when docs change. Only
  `check-see-paths` needs the always-on lane, because it alone guards docs refs
  against deletions/renames *elsewhere* in the tree. (`check-code-doc-anchors`
  technically shares this exposure for source-side `principles.md#anchor`
  citations; moving it to `checks.yml` is a natural follow-up, out of scope here.)

## Explicit non-goals (do NOT touch)

- **`002:65` and `003:53`** -- already correct git-history-note form. An earlier
  audit pass mislabeled these as dead pointers to "fix"; they are the gold
  standard. Leave verbatim.
- **The bodies of 002 / 007 / 008** -- frozen historical narrative. The dead
  `init-disk` / `apply` / `braid-add-disk` prose stays; it is the record of what
  the decision was at the time.
- **`002:63`** `` `cli/src/` — Rust CLI (`init-disk`, `plan`, `apply`, `status`) ``
  -- the path `cli/src/` still resolves, so this is a *live* bullet with a dated
  parenthetical, not a dead pointer. Updating the command list would be
  repointing-to-current (the anti-pattern); dropping the parenthetical would edit
  frozen narrative. Leave it. (This is the distinction the new convention encodes:
  remove dead navigation targets, not dated descriptions on live ones.)
- **`docs/dev/overview.md`** `braid-add-disk` mentions -- these are live VM test
  names (`tests/cli/braid-add-disk.nix` exists), not the deleted script. Out of
  scope.
- **`021-wait-in-unlock.md:100`** `` `cli/src/unlock.rs:93-96` `` -- a *live* path
  carrying a line-number suffix. It violates the `File References` rule (no line
  numbers) but is a different class from this plan's dead-pointer scope: the path
  resolves, and the new check strips `:93-96` and passes it. Flagged, not fixed --
  a proper fix means re-citing the "already-mounted short-circuit" by symbol, and
  the line numbers appear to have already drifted (93-96 now span a comment
  block), which is itself evidence for the line-number ban.

## Verification

- `rg -n 'braid-add-disk\.sh' docs/` -> no hits remain.
- `rg -n 'design-docs/1-braid-add-disk\.md' docs/` -> single hit, now carrying the
  `last present at commit 4112e57` note.
- `rg -n '9df91f9' docs/design/decisions/00{2,3}*.md` -> 002:65 and 003:53 still
  present and unchanged.
- `mdbook build docs` -> succeeds (no new broken cross-links; the edits touch only
  code-span paths and prose, and `mdbook-linkcheck2` stays green).
- `python3 scripts/docs/check-code-doc-anchors.py` and the other
  `scripts/docs/check-*.py` checks -> pass (the git-noted bullet stays a bare-path
  code span in the same passing category as 002:65/003:53; frontmatter untouched).
- `python3 scripts/docs/check-see-paths.py` -> passes on the fixed tree (also
  `just check-docs-see-paths`). Spot-checks it must pass: ADR 001:59's two
  pre-dash test paths both validated; 022:141 `plans/impl/...` (git-tracked) and
  024:300-303 bare-path bullets resolve; the git-noted 002:65 / 003:53 / 004:48
  lines are exempt; and post-dash non-paths are *not* resolved -- 002:63's
  `init-disk`, 003:51's `braid-online.service` / `braid-pool.target`, and 002:61's
  `braid.disks` must not be treated as files (they would false-positive without
  the target-cluster rule). Regression-test both ways: re-add a deleted
  `braid-add-disk.sh` bullet -> red; remove it -> green.
- The new `checks.yml` lane fires on a non-docs change: push a branch that only
  deletes a `## See`-referenced source file and confirm `checks.yml` (not
  `docs.yml`) goes red.
- Eyeball the three trimmed `## See` sections: each still has its remaining live
  bullets and no dangling blank line.

## Out of scope

No Rust/CLI/module changes. The only non-docs additions are the new docs-lint
script (`scripts/docs/check-see-paths.py`) and its wiring -- a `justfile` recipe
and a new always-on `.github/workflows/checks.yml`. That check *is* the
regression test for the doc edits, so no separate test harness is added for it
(consistent with the sibling `check-*.py`, validated by running in CI, not
unit-tested). The unstable/parser lanes, VM tests, and Rust tests are unaffected.
