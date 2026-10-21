---
name: pick-fix
description: >
  Walk the user through pitched review findings in rank order, record the
  fix they choose for each, and start an implement run per decision in the
  background. Use after the review workflow, or to re-decide blocked findings.
argument-hint: "[run-id] [--deferred]"
---

# pick-fix

The one human step of the review pipeline (`plan/2026-10-21-1047/D7`):
the user picks a fix per finding, and each pick starts the `implement`
workflow for that finding at once. All state goes through
`python3 scripts/findings/findings.py` (`FINDINGS` below); never edit
`findings/` by hand.

## 1. Collect the queue

Runs are the directories under `findings/` (in the main checkout). Use
the run id argument if given, else every run.

For each run, call `FINDINGS list <run> --state pitched --state blocked`
(add `--state deferred` with `--deferred`). Order the queue: blocked
findings first (they stopped work the user already chose), then the rest
by score, highest first. If the queue is empty, say so and go to step 4.

## 2. Pitch each finding

For each finding, read `FINDINGS show <id>` and print the pitch in the
terminal, in this order and in plain words:

1. Title, id, category, severity, confidence, effort.
2. The issue, then the claim's evidence in a line or two.
3. For a blocked finding: the earlier decision and why it was blocked.
4. Candidates, best design first. Mark the ideal and the recommended
   one. For each: summary, trade-offs, worst case.
5. Rejected solutions and why each lost.
6. The recommendation and its justification.
7. Dependencies (`depends_on`) and their states.

Then ask with `AskUserQuestion` (one question per finding):

- One option per candidate, labelled with its title, the recommended
  one first with "(Recommended)". Put its summary, trade-offs, and worst
  case in the option's `preview`.
- Add `Defer` and `Drop` options while the 4-option limit allows. With
  3 or more candidates, replace them with a single `Defer or drop`
  option and ask a follow-up question. With more than 4 candidates, show
  the top ones and say the rest can be chosen by typing their key in
  "Other".

Map the answer:

- A candidate: `FINDINGS decide <id> --candidate <key>`, with
  `--note "<text>"` if the user added notes.
- "Other" text naming a candidate key: that candidate, with the rest of
  the text as the note. "defer" or "drop": that action. Anything else:
  `FINDINGS decide <id> --note "<text>"`, a decision with no candidate.
- Defer or drop: `FINDINGS defer <id>` / `FINDINGS drop <id>`, with
  `--note` if the user gave a reason.

If the script refuses, show its error and ask again; never work around
it.

## 3. Launch

Right after each `decide`, if every finding in its `depends_on` is
`merged`, start the implement run in the background and move on without
waiting:

```
Workflow({name: "implement", args: {id: "<id>"}})
```

If the name is not found (workflows register on reload), use
`Workflow({scriptPath: "<main checkout>/.claude/workflows/implement.js", args: {id: "<id>"}})`.

If a dependency is not merged yet, record the decision but do not
launch; say which dependency it waits on.

## 4. Pending launches

Before finishing (and first, when the queue is empty), list findings in
state `decided` across the runs. A `decided` finding has no branch for
its current decision, so no run has started it. Launch each one whose
dependencies are now merged, as in step 3. Launching twice is safe: the
workflow stops, resumes, or starts by the stored decision.

## 5. Report

End with a short table: id, title, what the user chose, and whether an
implement run was started. Progress is in `/workflows`; results land on
`master` as commits with a `Finding: <id>` trailer, or the finding goes
to `blocked` with a reason, ready for the next `/pick-fix`.
