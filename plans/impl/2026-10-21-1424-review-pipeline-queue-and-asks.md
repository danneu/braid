# Review pipeline: decide ahead, drain unattended, ask when it matters

Builds on `plan/2026-10-21-1047` (the review pipeline). Labels from that
plan are cited fully qualified; labels here are local.

## Problem

The first live run (review run `261021-1103`, 12 pitched findings)
exposed five gaps:

- **Deciding starts work at once.** `/pick-fix` launches an implement
  run per decision (`plan/2026-10-21-1047/D7`), so a decision session
  spawns as many parallel runs as decisions, with no cap. The user wants
  to decide many findings in one sitting and let the work proceed while
  away, without a burst of parallel sessions.
- **Ordering lives in nobody's store.** Three pitches said "land after X"
  in prose (004 after 001/005/006; 016 and 013 after 011/012), but
  `depends_on` is only set by the review's merge step, so nothing
  enforced it. One pitch's ideal (002) contradicted another's (001).
  Nobody reviewed the pitches before the user saw them.
- **Runs cannot surface news.** An implementer can only finish or end
  `blocked` with a bare reason (`plan/2026-10-21-1047/I4`). Things that
  can't be known at pitch time (a landed fix that already did half the
  work, a trade-off that proves different in practice) either get
  silently adapted or burn repair rounds. Run 010 adapted correctly to
  001's landed fix, then spent both repair rounds on commit messages that
  still described pre-001 code, and ended `blocked` with approved code.
- **A blocked run loses its work.** Any new decision gets a new branch
  (`plan/2026-10-21-1047/D4`, `/D12`), so unblocking 010 would redo
  27 minutes of approved work.
- **The runner relay corrupts data.** Workflows reach the store through
  a small runner agent asked to return stdout verbatim
  (`.claude/workflows/implement.js#sh`). For 009 it abridged the launch
  payload, dropped the `finding` object, and the run crashed. The vetter
  also judged branch commit messages (`git log master..HEAD`) that never
  land, while never seeing the squash message that does.

## Desired outcome

1. Review produces pitches that a critic has challenged, with ordering
   recorded as dependencies.
2. `/pick-fix` only records decisions; the user can decide everything at
   once.
3. One `drain` run implements the decided queue unattended, a few at a
   time, in dependency order.
4. A run that learns something that would change the user's call stops
   that finding and asks; the rest of the queue continues; the user is
   notified. An answer that keeps the decision resumes the same branch.

## Decisions

- D1. **Workflows read only short control fields from the store.** Every
  store command a workflow parses returns small JSON (ids, actions,
  branch, tree, proof kind, acceptance recipes, statuses). Agents that
  need the finding, pitch, decision, or ask text read it themselves with
  `findings.py show`. The workflow validates each parsed result's shape
  and fails loudly on a mismatch.
- D2. **Dependencies are editable after review.** A store command adds
  and removes `depends_on` edges on unmerged findings, refusing cycles
  and unknown ids. The review's merge step and the critic (D3) use it;
  so can the user via `/pick-fix`.
- D3. **Every pitch gets a critic before the user sees it.** After the
  pitch stage, one fresh critic per pitched finding reads that pitch and
  every other pitch in the run. It attacks: the ideal is not the
  simplest by-construction structure; a candidate is missing; the
  ranking or recommendation does not follow from the trade-offs; the
  pitch conflicts with another pitch; an ordering is needed. It records
  dependencies through D2. If it asks for changes, the pitcher revises
  once and the revision is stored; there is no second round. A pitch
  the critic could not review is reported as a gap and stays `verified`.
- D4. **`/pick-fix` decides only.** It never starts implement runs.
  It shows open asks (D6) first, then blocked, then pitched by score. It
  ends by offering to start a `drain` run.
- D5. **`drain` runs the decided queue.** A saved workflow that, until
  nothing is runnable and nothing is running: starts the `implement`
  workflow for each runnable finding, at most `args.max` (default 3) at
  a time, and starts the next one as each finishes. Runnable means
  `decided`, every dependency merged, and not named by an open ask
  (D6). It re-reads the store before every start, so decisions and
  answers recorded mid-drain are picked up. It ends with a summary:
  merged, asking, blocked, still waiting on dependencies.
- D6. **Runs ask instead of guessing.** Replaces
  `plan/2026-10-21-1047/I4`'s "block with a reason" as the only exit
  short of merging. The implementer (and the vetter, via the repair
  limit) stops the finding with an **ask** when it learns something that
  would change the user's call:
  - the chosen solution is impossible, or clearly worse than another;
  - a trade-off the user accepted proves materially different;
  - the work needs a choice the decision does not cover that changes
    observable behavior, an invariant, or an ADR;
  - it affects another queued finding.

  Anything with an obvious answer inside the decision, it decides and
  records in the landed commit message. An ask has the pitch's shape:
  what was found, options with trade-offs, a recommendation with
  justification, and the ids of other findings it affects. The finding
  moves to `blocked` with the ask attached; the branch and worktree are
  kept. `blocked` with a bare reason remains for failures that need no
  choice (an infrastructure fault, an exhausted repair budget the run
  cannot explain).
- D7. **Two ways out of `blocked`.** Refines
  `plan/2026-10-21-1047/D4` and `/D12`:
  - **answer**: the user's reply keeps the current decision (the same
    candidate, plus the reply as a note). The finding returns to
    `decided` under the same decision number, so the next run resumes
    its existing branch.
  - **decide**: a different candidate or direction. A new decision
    number and a fresh branch, as today.
- D8. **The vetter approves what lands.** It reviews the branch's tree
  diff against `master` and the squash commit message the run will
  land, not the branch's intermediate commits. A message-only problem is
  repaired by rewriting the landing message, never the branch history.
- D9. **The implementer checks the decision against current `master`.**
  Before working, it compares the decision's premises with `master` as
  it is now (findings may have landed since the pitch). An adaptation
  inside the decision is recorded in the landed message; one that
  changes the decision is an ask (D6).
- D10. **Asks and blocks notify the user.** When a finding stops with an
  ask or a block, the user gets a push notification naming the finding
  and the question. If a workflow agent cannot send one (PO6), the
  `drain` summary carries it and the main session relays it.
- D11. **The merge queue decides "already merged" under its own lock.**
  Repairs a gap in `plan/2026-10-21-1047/D9`: today the merged check
  runs before the queue lock is taken. So two runs of one finding can
  both pass it, and the second rebases onto the first's squash (its patch
  drops as already applied), keeps the same tree, and lands a second
  `Finding:` commit. The check, the rebase, and the commit happen under
  one hold of the queue lock, so whichever run lands second sees the
  trailer and stops as merged.
- D12. **Gates and the vetter judge a snapshot no implementer touches.**
  The candidate is a commit. The gate checks it out, detached, in its own
  worktree that no implementer works in, runs the proof and acceptance
  recipes there, and the vetter reviews that same commit. The
  implementation worktree is never an input to approval, so nothing an
  implementer does there, committed or not, can change a verdict.

## Invariants

- I10. No agent acts on store text relayed through another agent; the
  text an implementer, vetter, critic, or user sees is what the store
  holds.
- I11. A `drain` run never has more than `args.max` implement runs in
  flight, and never starts a finding with an unmerged dependency or one
  named by an open ask.
- I12. An implement run never changes the user's chosen solution, an
  accepted trade-off, or behavior beyond the decision on its own; each
  such discovery stops that finding with an ask, and only that finding.
- I13. An answer that keeps the decision resumes the same branch; work
  a run already did is never redone for it. A new decision still starts
  a fresh branch (`plan/2026-10-21-1047/I9` and `/I2` unchanged: every
  landed tree passes every gate and the vetter).
- I14. The vetter approves the exact tree and the exact commit message
  that land.
- I15. Every pitch shown by `/pick-fix` has passed a critic, or is
  marked as not critiqued.
- I16. `/pick-fix` never starts an implement run.

## Proof obligations

- PO1 (I10, D1): a script test shows every store command a workflow
  parses returns only control fields; a workflow fed a malformed result
  stops with an error instead of proceeding.
- PO2 (D2): script tests add and remove edges, and refuse cycles,
  self-edges, unknown ids, and edges on merged findings.
- PO3 (I13, D7): script tests: answer on a blocked finding keeps the
  decision number and branch, and launch reports `resume`; decide on a
  blocked finding starts a new number and branch; answer is refused on a
  finding that is not blocked.
- PO4 (I11, D5): a `drain` run over a scratch repo (as in
  `plan/2026-10-21-1047/PO3`) with five decided findings, one depending
  on another and one named by an open ask, and `max: 2`: never more than
  two runs in flight, the dependent starts only after its dependency
  merges, the held one never starts.
- PO5 (I12, D6, D9): on the scratch repo, a decided finding whose chosen
  solution is undermined by a commit landed after the decision ends as
  an ask with options; another finding in the same drain still merges.
  Answering it and draining again resumes the same branch and merges.
- PO6 (premise, D10): a workflow agent can send a push notification.
  Verify before building D10 on it; otherwise use the summary fallback.
- PO7 (I14, D8): on the scratch repo, a run whose branch commits carry a
  stale message but whose landing message is correct is approved and
  lands; a wrong landing message is rejected and repaired without
  rewriting branch commits.
- PO8 (I15, D3): a `review` run over one command slice shows a critic
  verdict per pitch, records any ordering it finds as dependencies, and
  reports a skipped critic as a gap.
- PO9 (I16, D4): `/pick-fix` makes no Workflow call except a `drain` the
  user accepts.
- PO10 (`plan/2026-10-21-1047/I9`, D11): a script test runs two `land`
  calls for one finding concurrently, both with the approved tree. Each
  waits on the queue lock while the other holds it. Exactly one
  `Finding:` commit reaches `master`, and the other call reports merged.
- PO11 (`plan/2026-10-21-1047/I2`, D12): a script test prepares a gate
  snapshot for a candidate commit whose test fails, then edits and
  commits in the implementation worktree. The snapshot still holds the
  candidate's tree, and its test still fails.

## Rollout for run 261021-1103

After the change lands: record dependencies (004 on 001, 005, 006; 016
on 011, 012, 014; 013 on 011, 012), answer 010's block to keep its
decision so it resumes its approved branch, and start one `drain`.

## Non-goals

- OOS1. Pushing, PRs, CI (as `plan/2026-10-21-1047/OOS1`).
- OOS2. Answering asks from the phone; the notification only says that
  something is waiting.
- OOS3. Scheduling drains on a timer.

## Accepted risks

- AR1. Two concurrent `drain` runs (or a drain plus a manual implement
  launch) can run the same finding twice in one worktree, and each can
  disturb the other's uncommitted work. Landing stays safe: approval
  reads only a snapshot neither run edits (D12), and D11 lands each
  finding once. The cost is wasted work or a spurious
  repair round. The user starts drains, and `/pick-fix` offers one only
  at its end.
- AR2. The critic adds one agent per finding to a review run. Pitches
  are what the user acts on, and run 261021-1103 had three ordering or
  conflict misses in twelve.

## Rejected ideas

- RI1. Keep launching at decision time with a global cap. Decisions and
  execution have different lifetimes; the cap would still need a queue,
  and a decision session would still have to stay open while it drains.
- RI2. Ask the user live from inside the run (block until answered). The
  user is often away; a stopped finding with a stored ask costs nothing,
  and the rest of the queue keeps moving.
- RI3. Exclusive per-finding run ownership held from launch through
  cleanup. A workflow has no long-lived process to hold a lock, so
  ownership would need a lease with expiry and takeover: a new state
  machine whose stale leases block resumes after a crash. D11 already
  makes the outcome that matters, a duplicate landing, impossible.

## Implementation discretion

- How the ask payload and answer note are stored, and whether `ask` is a
  sub-kind of `blocked` or a separate stored state, as long as D7's two
  exits and PO3 hold.
- Whether the gate snapshot is fresh per check or one per finding moved
  between candidate commits (the latter keeps the Rust build cache).

## Commit progress

- [x] C1. fix(findings): decide "already merged" under the merge-queue lock
- [x] C2. fix(review): workflows parse only validated control fields from the store
- [x] C3. feat(review): gate and vet a snapshot of the candidate and the message that lands
- [x] C4. feat(review): runs stop with an ask, and an answer resumes the same branch
- [x] C5. feat(review): drain the decided queue; /pick-fix only decides
- [ ] C6. feat(review): critique every pitch and record dependencies it finds

## Implementation notes

- C1: the PO10 test runs two `land` subprocesses while the test holds
  the queue lock, so both queue behind it. Against the old script, the
  second run failed outright: it had passed the merged check, then
  rebased in a worktree the first run had already removed. Worktree
  cleanup now also happens under the queue lock.
- C2: PO1's workflow half ran live. `implement` was fed a launch result
  missing four fields through a stub store command, and it stopped with
  a malformed-result error before creating anything.
- C3: one snapshot per candidate commit
  (`.claude/worktrees/snapshot-<id>-<commit>`), not one per finding moved
  between candidates. A per-finding snapshot would let a duplicate run
  (AR1) move it under a gate that is still running, which recreates the
  gap D12 closes. A new candidate's snapshot removes the finding's older
  ones, and landing removes the rest. The Rust build cache is lost per
  candidate; Nix builds are cached in the store anyway.
- C3: D8's message repair is done by the vetter. It returns the exact
  message to land: the implementer's message when accurate, otherwise
  its own rewrite. A message problem alone never rejects a candidate, so
  it never costs a repair round, and the message that lands is the one
  the vetter approved (I14).
- C3: PO7 ran live on a scratch repo. The branch held the right fix
  under a stale commit message ("rewrite the size parser"). The gate ran
  in the snapshot, and the vetter approved and returned the implementer's
  accurate message, which landed. Both worktrees were removed. The
  implementer wrote a correct message, so the vetter's rewrite path did
  not run live. It uses the same `message` field that landed here.
- C4: an ask is stored on the current decision (`decision.ask`), and
  `blocked` stays the only stored state. `answer` moves the open ask or
  bare reason into `decision.answers` with the reply. It works on any
  blocked finding, so 010's bare block can be answered too.
- C4: an open ask holds only `launch`. `worktree`, `snapshot`, and
  `land` ignore it, so a run already past launch can still land; I11
  is about starts.
- C4: the implementer records its own ask through `findings.py ask`,
  as review agents do, so the text never passes through a relay
  (I10). The workflow then confirms through `launch` that the finding
  is blocked before reporting `asking`.
- C4: when repairs run out, the implementer gets one more turn with the
  last rejection and may not change the branch. It records an ask if a
  choice by the user would resolve the rejection; otherwise the run
  blocks with the bare reason, as D6 allows.
- C5: "runnable" is computed by a new store command, `findings.py
  queue`, rather than in the drain script. It is deterministic and
  unit-tested, and it returns ids only (D1). `drain` reads it before
  every start. It runs each finding at most once per drain, so a run
  that ends in a crash or a land wait does not loop.
- C5: PO6 holds. A haiku workflow agent loaded `PushNotification`
  through ToolSearch and sent one ("Terminal notification sent. Mobile
  push requested."). `implement` sends one on every ask and block.
  `args.notify: false` turns this off for scratch-repo runs, and the
  drain summary carries the same news either way.
- C5: PO4 and PO5 ran live as one scratch drain with `max: 2`, over
  five decided findings. Concurrent `workflow()` calls worked: at most
  two were in flight. The dependent finding started only after its
  dependency merged, and the finding held by an open ask never started.
  The rename finding saw a contract that landed after its decision and
  asked with three options instead of renaming. The other three
  findings still merged. After `answer` kept the decision, a second
  drain launched it as `resume` on the same `review/<id>/1` branch and
  merged it.
- C5: PO9 is met by the skill's text, not a live session, because
  `/pick-fix` needs the user to answer. The only Workflow call it
  describes is the `drain` the user accepts at the end.
