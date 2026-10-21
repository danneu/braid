---
intent: How the agent review pipeline finds, verifies, pitches, implements, and lands improvements -- the finding store, the review, drain, and implement workflows, /pick-fix, asks, and the merge queue.
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
| Critique | `review` workflow | a critic attacks each pitch against the run's other pitches; one revision; orderings recorded |
| Pick | you, via `/pick-fix` | choose a candidate, defer, or drop; answer asks |
| Drain | `drain` workflow | start an `implement` run per runnable finding, a few at a time |
| Implement | `implement` workflow | worktree, proof, fix, gates, vetter, merge queue |

## Running it

- Review everything: run the `review` workflow. Narrow it with
  `args: {slices: ["command:add", "docs:guides/*", "cross:parsers"], focus: "..."}`.
  Slice ids are `command:<name>`, `docs:<path under docs/>`, and
  `cross:<lens>`; a trailing `*` matches a prefix. The lenses are listed in
  `.claude/agents/reviewer.md`.
- Choose fixes: `/pick-fix` (optionally a run id). It shows open asks
  first, then blocked findings, then pitches by rank, and records each
  decision without starting any work. Decide as many as you like in one
  sitting.
- Implement them: `/pick-fix` ends by offering a `drain` run, or start
  one yourself (`args: {max: 3}`). It starts an `implement` run for each
  runnable finding (decided, every dependency merged, not held by an
  open ask), at most `max` at once, and starts the next as each ends. It
  re-reads the store before every start, so decisions and answers you
  record while it runs are picked up. It ends with a summary: merged,
  asking, blocked, and still waiting. Run one drain at a time; watch it
  in `/workflows`. `findings.py queue` shows the same lists.
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
built for an earlier choice. An answer to a blocked finding keeps its
decision and branch (see below).

Before you see a pitch, a critic reads it alongside every other pitch in
the run. It checks that the ideal really is the simplest structure that
rules the problem out, that no candidate is missing, that the
recommendation follows from the trade-offs, and that no pitch conflicts
with another. When one fix must land after another, it records the
ordering with `findings.py depends`, which refuses cycles. You can edit
dependencies the same way. A `revise` verdict gets one revision, and
each pitch shows its critique or says "Not critiqued."

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
a bounded number of repair rounds. Relaunching any finding is safe: the
run stops, resumes, or starts by the stored decision.

## Asks

A run never changes your chosen fix, an accepted trade-off, or behavior
beyond the decision on its own. Before working, the implementer checks
the decision against `master` as it is now, since other fixes may have
landed. When it learns something that would change your call (the fix
is impossible or clearly worse, a trade-off proves different, the work
needs a choice the decision does not cover, or another queued finding
is affected), it stops that finding with an **ask**: what it found,
options with trade-offs, a recommendation, and the findings it affects
(`findings.py ask`). The finding goes to `blocked` with its branch kept,
and the findings the ask names do not start until it is answered. You
get a push notification naming the finding and the question, and the
drain summary lists it too.
Repairs that run out become an ask when a choice would resolve them;
otherwise, and for failures that need no choice, the finding is
`blocked` with a bare reason.

There are two ways out of `blocked`:

- **answer** (`findings.py answer <id> --note ...`): keep the decision,
  with your reply added to it. The next run resumes the same branch.
- **decide**: a different candidate or direction. A new decision number
  and a fresh branch.

VM acceptance recipes take a host-wide gate, so parallel implement runs
boot VMs one invocation at a time
([testing](testing.md#vm-booting-recipes-share-a-host-wide-gate)).
