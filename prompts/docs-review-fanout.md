# Docs Review Fan-Out

Build the page manifest before spawning reviewers. The authoritative TOC
is `docs/SUMMARY.md`; every content page is tracked and listed there.
Derive the manifest from tracked files, excluding the TOC itself:

```sh
git ls-files 'docs/*.md' 'docs/**/*.md' | grep -vx 'docs/SUMMARY.md'
```

Expect 69 pages. Verify the count, and verify the only tracked
`docs/**/*.md` file absent from `SUMMARY.md` is `SUMMARY.md`:

```sh
comm -23 <(git ls-files 'docs/*.md' 'docs/**/*.md' | sed 's#docs/##' | sort) \
         <(grep -oE '\(([a-zA-Z0-9/._-]+\.md)\)' docs/SUMMARY.md | tr -d '()' | sort)
```

If that prints anything other than `SUMMARY.md`, a page is missing from
the TOC -- stop and report it rather than silently skipping it.

The seven `SUMMARY.md` sections and their page counts: Landing
(`index.md`, 1), Guides (13), Commands (16), Design (`principles.md`, 1),
Decisions (27), Internals (8), Development (3).

Pre-flight is verify-only except for the findings directory:

```sh
mkdir -p docs-findings
test -f .claude/agents/docs-reviewer.md
jq empty .claude/settings.json
```

Abort if the agent file is missing or the committed shared settings file
is invalid. The caller must already have local mutating permissions for
`docs-findings/`; do not mutate Claude settings at runtime.

## Progress tasks

Reviewers are per-page (one subagent per doc, 69 total), but progress
tasks are per-section so the task list stays navigable. Before fan-out,
issue 8 `TaskCreate` calls in `pending` state: one titled `Review docs:

<section>` for each of the 7 sections (Landing, Guides, Commands, Design,
Decisions, Internals, Development) and one titled `Roll up findings into
docs-findings/_index.md`. Capture every `taskId` and maintain a local
`section -> taskId` mapping.

## Fan-out

Spawn one `docs-reviewer` per page via parallel `Agent` tool calls with
`subagent_type: "docs-reviewer"`. 69 reviewers exceeds the harness
parallelism cap, so spawn in waves of about 10 in `SUMMARY.md` order. In
the same assistant message that spawns a section's first wave, call
`TaskUpdate` to mark that section's task `in_progress`.

Each prompt should be only:

```text
Doc page: docs/{PATH}
Representative starting files:
{FILES}
```

Choose `{FILES}` as useful starting points for the code or behavior that
page documents -- the subagent owns full discovery. Mapping:

- `docs/commands/<cmd>.md` -> `cli/src/main.rs` dispatch, the matching
  `cli/src/<module>.rs`, any shared planner/executor it calls, and
  `README.md`.
- `docs/guides/*.md` -> the relevant `cli/src/*.rs` and
  `modules/braid/*.nix`, and `README.md`.
- `docs/design/principles.md` -> the invariants it asserts, across
  `cli/src/` and `modules/braid/`.
- `docs/design/decisions/NNN-*.md` -> the code that implements or is
  governed by that decision.
- `docs/internals/**` -> the specific subsystem in `cli/src/` plus the
  upstream tool source in `reference/`.
- `docs/dev/*.md` -> `tests/`, `flake.nix`, `justfile`, `scripts/`.
- `docs/index.md` -> `README.md` and `docs/SUMMARY.md`.

As each reviewer returns
`Wrote ./docs-findings/<slug>.md. Top finding: <one line>.`, record it.
When every page in a section has returned, mark that section's task
`completed` before its successors. If a subagent fails or returns
malformed output, still count it as returned and note the failure in that
section's task body.

## Slug scheme

`<slug>` is the page path relative to `docs/`, with `.md` dropped and `/`
replaced by `-`. Examples: `docs/index.md` -> `index`;
`docs/guides/auto-unlock.md` -> `guides-auto-unlock`;
`docs/commands/ups-status.md` -> `commands-ups-status`;
`docs/design/decisions/024-luks-uuid-identity.md` ->
`design-decisions-024-luks-uuid-identity`;
`docs/internals/btrfs/balance-soft.md` -> `internals-btrfs-balance-soft`.

## Rollup

After all 7 section tasks are completed, verify the findings set is
complete and the run modified nothing tracked:

```sh
ls docs-findings/ | wc -l                                # must be 69
git check-ignore -q docs-findings/index.md && echo "isolated (gitignored)"
git status --porcelain                                   # only pre-existing changes
```

`docs-findings/` is gitignored, so it never shows up in `git status` -- that
is expected and is NOT a pass signal on its own. The real gates are the
69-file count and a `git status` that lists only changes which predate this
run. Reviewers are instructed not to modify tracked files; if one did, it
appears here -- investigate and discard that edit before rolling up. Then
mark the rollup task
`in_progress` and spawn one `general-purpose` rollup agent to read the 69
findings files and write `docs-findings/_index.md` -- not `index.md`,
which is the landing page's own per-page findings file (slug `index`).
The rollup may not
create new findings or reinterpret existing ones; copy each one-line
`Issue` text verbatim into a severity-sorted table, High before Medium
before Low.

Columns, left to right: `Status | Severity | Category | Page | Finding | Issue`.

- `Status` initializes to a single space `" "` for every row.
- `Severity` is `High`, `Medium`, or `Low`, copied from the source
  finding.
- `Category` is `Accuracy`, `Consistency`, `Completeness`, or `Clarity`,
  copied from the source finding.
- `Page` is the doc path relative to the repo (e.g.
  `docs/guides/auto-unlock.md`).
- `Finding` links to the per-page file at the source finding's line,
  formatted `[#N](./<slug>.md:<lineno>)`. `<lineno>` is the line number
  in `docs-findings/<slug>.md` of the standalone line matching `^\(N\)$`
  -- the line that introduces finding `(N)`. For example, if `(3)` is on
  line 147 of `guides-auto-unlock.md`, the cell is
  `[#3](./guides-auto-unlock.md:147)`.
- `Issue` is the one-line `Issue:` text from the source finding, verbatim.

When the rollup returns its one-line
`Wrote ./docs-findings/_index.md. Top finding: <one line>.` result, mark
the rollup task `completed`.
