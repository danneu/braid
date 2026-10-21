# Review pipeline overhaul

## Problem

The review fan-out (`prompts/command-review-fanout.md`,
`prompts/docs-review-fanout.md`, `prompts/verify-findings-fanout.md`,
`.claude/agents/command-reviewer.md`, `.claude/agents/docs-reviewer.md`)
was written for weaker models and a human-driven loop:

- The main session drives orchestration from prose: hand-counted task
  lists, waves of spawns, manual retries. A dropped reviewer is easy to
  miss, and a crashed run cannot resume.
- Reviewers write free-form markdown. A rollup agent re-parses it,
  including a `^\(N\)$` line-number lookup.
- Status lives in a hand-aligned markdown table. Agents flip cells by
  matching exact runs of spaces, and danterm tab colour decides "done".
- Nothing checks findings before ranking. False positives get ranked
  and only die when someone tries to implement them.
- Slices are per command or per doc page. Bugs, simplifications, and
  pivots that span several areas (module + CLI, parsers, subprocess
  layer, test suite) belong to no reviewer.
- Each finding offers one fix. The user never sees the alternatives or
  why they lost, so the real decision happens implicitly.

## Desired outcome

One pipeline, from review to merged commit. The only human step is
choosing a solution for each finding:

1. **Review** -- reviewers fan out over slices and return
   schema-validated findings.
2. **Verify** -- a skeptic tries to refute each finding.
3. **Rank** -- findings that several slices report are merged, and
   the rest are scored.
4. **Pitch** -- each surviving finding gets a pitch: the issue, rejected
   solutions with reasons, ranked candidate solutions with trade-offs,
   and a recommendation with justification.
5. **Pick fix** -- an interactive session walks the user through the
   pitches in rank order. Each decision launches step 6 for that
   finding at once.
6. **Implement -> vet -> automerge** -- each finding runs in its own
   branch and worktree, unattended, until it lands on `master` or stops
   as `blocked` with a reason.

## Decisions

- D1. **Orchestration lives in saved Workflow scripts** under
  `.claude/workflows/`: `review` (steps 1-4) and `implement` (step 6,
  one finding per run). Control flow is code, so it is deterministic,
  resumable, and coverage gaps are logged rather than silent. A
  parameter (`args`) replaces the "Extra focus" section that is hand-edited
  today: slice filter and focus text.
- D2. **One file per finding, under a script that owns all state.**
  `findings/<run-id>/<finding-id>.md` is YAML frontmatter (machine fields)
  plus a markdown body (pitch, decision, implementation log). The
  directory is gitignored. `scripts/findings/` is the only writer: it
  ingests workflow output, generates the ranked `index.md`, and changes
  status. Agents and skills call it; they never edit files in the store
  by hand. All deterministic pipeline logic lives in the script, not the
  workflows: dropping refuted findings, applying duplicate merges,
  ranking, recording decisions, and choosing the solution to launch.
  The workflows only route agent output into it.
- D3. **Finding schema.** `id`, `title`, `slice`, `category` (correctness,
  simplification, performance, test-gap, docs, pivot), `severity`,
  `confidence`, `effort`, `locations`, `claim`, `evidence`, `proof`,
  `acceptance` (the `just` recipes that must pass), `depends_on`,
  `status`. The script computes rank from severity, confidence, and
  effort; agents never assign rank. `proof` is the evidence the
  category requires:
  - correctness and test-gap: a regression check that fails before the
    fix and passes after it;
  - performance: a before/after benchmark;
  - simplification, docs, and behavior-preserving pivots: a check that
    behavior is preserved (the existing tests that cover the touched
    code, or a doc check) plus vetter review. No red run is required.
- D4. **Status is a closed state machine, and git owns the
  implementation states.** The store holds
  `open -> verified | refuted`; `verified -> pitched`;
  `pitched -> decided | deferred | dropped`;
  `deferred -> decided | dropped`; `decided -> blocked`;
  `blocked -> decided` (the user re-decides). The script rejects any
  other transition. Every `decided` entry starts a new numbered
  decision. Git, not the store, holds "implementing" and "merged". A
  finding is implementing when it is `decided` and the branch for its
  current decision, `review/<finding-id>/<decision-number>`, exists. It
  is merged when `master` contains a commit carrying a
  `Finding: <finding-id>` trailer. The script derives both whenever it
  reads status. So the fast-forward itself records the merge, and no
  crash can leave the store and git disagreeing about it. Branches from
  earlier decisions stay as history, and no later run resumes them.
- D5. **One reviewer agent, lens per slice kind.** It replaces
  `command-reviewer` and `docs-reviewer`. Slice kinds: `command` (one per
  public leaf command), `docs` (one per page in `docs/SUMMARY.md`), and
  cross-cutting slices: NixOS module + systemd units, parsers + fixtures,
  subprocess layer, dry-run planner/executor, test suite (cost,
  redundancy, tests that assert nothing), architecture/pivots, and
  performance of the status/TUI/monitor paths. The workflow discovers
  the slice set from the code and `docs/SUMMARY.md` at run time; nothing
  is hard-coded.
- D6. **The pitch carries the design bar.** Every pitch names the ideal
  solution (the simplest structure in which the problem cannot happen),
  even when it doesn't rank first. Candidates are ranked by which is the
  right design, never by effort. Each candidate states its worst
  realistic case. A finding whose verified best move is "do nothing" is
  pitched that way, so the user can still drop it in one keystroke.
- D7. **Choosing fixes is a project skill (`/pick-fix`).** It walks `pitched`
  findings in rank order and shows each pitch in the terminal. The
  user chooses with `AskUserQuestion`: one option per candidate, with
  trade-offs in the preview, plus defer/drop. Free text through
  "Other" becomes part of the decision. It records the decision and
  starts the `implement` workflow for that finding in the background,
  then moves on without waiting.
- D8. **Implement cycle per finding.** A fresh worktree on branch
  for the current decision (D4), cut from `master`'s tip. The implementer
  produces the proof its category requires (D3), applies the chosen
  solution, and runs the acceptance recipes. A fresh vetter
  agent, with no access to the implementer's reasoning, reviews the diff
  against the decision, `docs/design/principles.md`, and the relevant
  ADRs. A bounded number of repair rounds follows; when they run out,
  the finding goes to `blocked`.
- D9. **Automerge is a local, serialized merge queue.** When a branch
  passes, it rebases onto `master`'s tip. If the rebase or a repair
  round changed the candidate's tree, the candidate goes through every
  gate I2 requires again, vetter included. Then the queue fast-forwards
  `master`. While the main checkout has uncommitted changes, the queue
  waits, reports, and retries later.
- D10. **VM-booting recipes take a host-wide gate.** VM tests run their
  drivers on the Mac itself (QEMU with HVF, see `justfile#_build-checks`).
  One recipe already runs several VMs at once, capped by its own
  `--max-jobs`. Every recipe that boots VMs takes one shared gate, so
  separate invocations (from concurrent implement runs, or the user)
  run one at a time. Concurrency inside a recipe stays as it is.
  Cheaper gates (Rust tests, clippy, doc checks) run in parallel.
- D12. **Restart obeys the stored decision, then git.** The
  `implement` workflow reads the finding's state (D4) before any side
  effect, and the first rule that matches wins:
  1. merged: stop and do nothing;
  2. not `decided` (for example `blocked`): stop and do nothing until
     the user decides again;
  3. the current decision's branch exists: resume from it, and let the
     gates decide what work is left;
  4. otherwise: start that branch fresh.

  Launching the same finding twice, or restarting a run after any
  crash, never applies a change twice. It never continues work the
  user has not authorized.
- D11. **Retire the old system in the same change.** Delete the three
  prompt files and the two reviewer agents, and drop the
  `command-findings` and `docs-findings` gitignore entries. Document the
  pipeline in `docs/dev/` (linked from `SUMMARY.md`) and add an AGENTS.md
  "read before you touch" line.

## Invariants

- I1. Only `scripts/findings/` writes to the finding store, and only
  legal D4 transitions succeed.
- I2. A commit reaches `master` only if all of these hold for the
  exact tree being merged:
  - the proof its category requires (D3) holds, including the red run
    before the fix for correctness and test-gap findings;
  - every acceptance recipe is green;
  - the vetter approved that tree.
- I3. `master` moves only by fast-forward from the merge queue, and only
  while the main checkout is clean. Nothing is ever pushed.
- I4. The implementer applies only the solution the user chose. If that
  solution proves wrong or impossible, the finding goes to `blocked`
  with the reason. The agent never swaps in another solution.
- I5. Review, verify, rank, and pitch never modify tracked files.
- I6. Every pitch includes the ideal solution, at least one rejected
  solution with its reason, and a recommendation with justification.
- I7. A failed or skipped agent leaves its finding in its prior state,
  and the run logs the gap. A partial run never reports full coverage.
- I8. At most one VM-booting recipe invocation runs on the host at a
  time.
- I9. Each finding lands on `master` at most once, however many times
  its implement run is launched, restarted, or interrupted.

## Proof obligations

- PO1 (I1, D4): script tests cover every legal transition and reject
  the illegal ones. Derived states follow git: a branch makes a finding
  implementing, and a trailer reachable from `master` makes it merged.
- PO7 (D2): deterministic script tests fed with canned agent results
  show that:
  - refuted findings never reach `pitched`;
  - duplicate merges collapse into one finding;
  - rank responds to severity, confidence, and effort;
  - the recorded decision is exactly the solution that launch selects.
  Live-agent runs (PO2, PO3) are smoke checks on top of these.
- PO2 (I5, I7, D5): a `review` run filtered to two command slices and
  one docs slice produces valid finding files and an index, and leaves
  `git status` unchanged. A slice whose reviewer is skipped shows up as
  a logged gap.
- PO3 (I2, I4, D3, D8): run `implement` on a scratch branch with
  hand-written `decided` findings:
  - a planted-bug correctness finding merges;
  - a correctness finding whose proof cannot go red ends `blocked`;
  - a simplification finding with no red test merges on preservation
    checks and vetter approval.
- PO4 (I2, I3, D9): the merge queue refuses to move `master` while the
  main checkout is dirty, and merges after it is cleaned. With two
  competing branches, the second rebases onto the first. If its repaired
  candidate passes acceptance but the vetter rejects it, it does not
  merge.
- PO5 (I8, D10): with lightweight substitute commands in place of VM
  tests, two concurrent invocations through the shared gate take turns,
  and concurrency inside one invocation is unchanged.
- PO8 (I9, D4, D12): interrupt an implement run at two points: right
  after launch (branch created, no commit yet), and after the
  fast-forward but before the run reports. Restarting each one finishes
  the finding with exactly one `Finding:` commit on `master`.
  Restarting a `blocked` finding whose branch still exists starts no
  implementation and merges nothing. After the user decides again,
  the new run works on a fresh branch for the new decision, not the
  blocked one.
- PO6 (premise): saved workflows in `.claude/workflows/` can be started
  by name from a skill and run in the background. Verify this before
  building D7 on it.

## Non-goals

- OOS1. Pushing to GitHub, opening PRs, or touching CI.
- OOS2. Migrating findings from the old `command-findings/` and
  `docs-findings/` runs.
- OOS3. A web UI for picking fixes.

## Accepted risks

- AR1. A fix the vetter wrongly approves lands on `master` unattended.
  Mitigations: I2's category proof, an independent vetter, and a local
  `master` that is never pushed, so a revert costs nothing.
- AR2. Overlapping findings conflict at rebase. The queue gives the
  implementer one rebase-repair round, then sends the finding to
  `blocked`.

## Rejected ideas

- RI1. Keep prose-driven fan-out in the main session. It can't resume,
  it hides dropped agents, and it fills the main context.
- RI2. A tracked or hand-edited status table. Alignment-sensitive
  edits are fragile, and per-finding files make a generated index
  correct by construction.
- RI3. Merging through an integration branch the user merges later.
  The user asked for automerge, and AR1's local-only `master` already
  makes reverts cheap.

## Implementation discretion

- Finding ID format, run-ID format, and the script's language
  (the existing `scripts/docs/` checks are Python).

## Commit progress

- [x] C1. feat(findings): add the script-owned finding store with git-derived status
- [x] C2. feat(just): serialize VM-booting recipe invocations through a host-wide gate
- [ ] C3. feat(review): add the review workflow and unified reviewer agent
- [ ] C4. feat(review): add the implement workflow with vetting and the local merge queue
- [ ] C5. feat(review): add /pick-fix to decide pitches and launch implement runs
- [ ] C6. chore(review): retire the prompt-driven fan-out and document the pipeline

## Implementation notes

- C1: added `deferred -> decided | dropped` to D4. As written, `deferred`
  had no way out, so a deferred finding could never be picked up again.
- C1: added a one-line `title` to the D3 schema. The index and `/pick-fix`
  need a short label, and `claim` is a paragraph.
- C1: finding IDs are `<run-id>-NNN`, where the run ID is `YYMMDD-HHMM`
  in local time. IDs are unique across runs, so the `Finding:` trailer
  and the decision branch name both identify a finding without naming
  its run.
- C1: the store lives in the main checkout's `findings/`, found through
  `git rev-parse --git-common-dir`. Implement runs in linked worktrees
  therefore share one store.
- C1: a duplicate merge deletes the absorbed files and records their IDs
  and locations on the keeper. D4 has no "duplicate" state, and adding
  one would only keep files nobody acts on.
- C2: the gate is an flock held by the exec'd command (`scripts/vm-gate/vm_gate.py`),
  so the kernel releases it when the command exits or crashes. It covers
  every justfile command that builds checks or runs a VM, including
  `playground`: an open playground makes implement runs wait, which is
  what I8 asks for. A test fails on any ungated `nix build` or `nix run`
  line that isn't a package or nixpkgs tool.
