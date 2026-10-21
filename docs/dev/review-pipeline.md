---
intent: How the agent review pipeline finds, verifies, pitches, implements, and lands improvements -- the finding store, the review and implement workflows, /pick-fix, and the merge queue.
---

# Review pipeline

An agent pipeline that reviews braid, lets you choose a fix per finding,
and lands each choice on `master` unattended. Design and invariants:
`plan/2026-10-21-1047` (`plans/impl/2026-10-21-1047-review-pipeline-overhaul.md`),
revised by `plan/2026-10-21-1424` (`plans/impl/2026-10-21-1424-review-pipeline-queue-and-asks.md`).

| Step | Who | What |
|---|---|---|
| Review | `review` workflow | one reviewer per slice records findings |
| Verify | `review` workflow | a skeptic tries to refute each finding |
| Merge | `review` workflow | duplicates across slices collapse; dependencies recorded |
| Pitch | `review` workflow | ideal fix, ranked candidates, rejected options, recommendation |
| Pick | you, via `/pick-fix` | choose a candidate, defer, or drop |
| Implement | `implement` workflow | worktree, proof, fix, gates, vetter, merge queue |

## Running it

- Review everything: run the `review` workflow. Narrow it with
  `args: {slices: ["command:add", "docs:guides/*", "cross:parsers"], focus: "..."}`.
  Slice ids are `command:<name>`, `docs:<path under docs/>`, and
  `cross:<lens>`; a trailing `*` matches a prefix. The lenses are listed in
  `.claude/agents/reviewer.md`.
- Choose fixes: `/pick-fix` (optionally a run id). Each decision starts an
  `implement` run in the background; watch them in `/workflows`.
- Read results: `findings/<run>/index.md` ranks a run's findings; each
  finding is `findings/<run>/<id>.md`. A landed fix is one commit on
  `master` with a `Finding: <id>` trailer.

## The finding store

`findings/` in the main checkout is gitignored. Every change goes
through `python3 scripts/findings/findings.py`, which validates payloads
and allows only legal status changes (`just test-findings`). Never edit
the files by hand.

The stored states are `open`, `verified`, `refuted`, `pitched`,
`decided`, `deferred`, `dropped`, and `blocked`
(`scripts/findings/findings.py#TRANSITIONS`). Two states come from git
on every read: `implementing` (the branch `review/<id>/<decision>`
exists) and `merged` (a `Finding: <id>` trailer is reachable from
`master`). Each new decision gets a new branch, so no run resumes work
built for an earlier choice.

Rank is computed from severity, confidence, and effort
(`scripts/findings/findings.py#score`); agents never assign it.

A workflow script cannot run processes, so it reaches the store through
a small runner agent that relays stdout. The commands a workflow parses
(`launch`, `worktree`, `block`, `land`) return only short control
fields, and the workflow checks each result's shape and stops on a
mismatch. Agents that need a finding's text, pitch, or decision read it
themselves with `findings.py show`, never through a relay.

## What lands, and when

An implement run lands a change only when, for the exact tree being
merged:

- the category's proof holds: a test that fails before the fix and passes
  after it (correctness, test-gap), a benchmark (performance), or the
  existing checks for behavior-preserving changes;
- every acceptance recipe is green;
- a fresh vetter, who never saw the implementer's reasoning, approved it
  together with the commit message that lands.

The gates and the vetter judge a detached snapshot of the candidate
commit (`findings.py snapshot`), never the implementation worktree, so
nothing an implementer does there can change a verdict. The vetter
reviews the change against `master` and the squash message, not the
branch's own commits, which never land; when only the message is wrong,
it rewrites the message instead of rejecting the change.

The merge queue (`findings.py land`) then squashes the branch into one
commit on `master` with that message. If `master` moved, it rebases and sends the new tree
back through every gate. It waits while the main checkout has
uncommitted changes, and nothing is pushed. It checks for an earlier
`Finding:` commit under its own lock, so two runs of one finding land it
once. A failed gate or vetter gets
a bounded number of repair rounds; after that, or when the chosen fix
proves impossible, the finding goes to `blocked` with a reason and waits
for your next `/pick-fix`. Relaunching any finding is safe: the run
stops, resumes, or starts by the stored decision.

VM acceptance recipes take a host-wide gate, so parallel implement runs
boot VMs one invocation at a time
([testing](testing.md#vm-booting-recipes-share-a-host-wide-gate)).
