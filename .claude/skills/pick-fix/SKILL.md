---
name: pick-fix
description: >
  Walk the user through review findings that need them -- open asks,
  blocked runs, then pitches in rank order -- and record each decision or
  answer. Never starts implement runs itself; it ends by offering one
  drain run that implements the decided queue. Use after the review
  workflow, or when an implement run asked a question or got blocked.
argument-hint: "[run-id] [--deferred]"
---

# pick-fix

The one human step of the review pipeline (`plan/2026-10-21-1047/D7`,
revised by `plan/2026-10-21-1424/D4`): the user decides, and a `drain`
run does the work later, a few findings at a time. This skill never
starts an `implement` run (`plan/2026-10-21-1424/I16`). All state goes
through `python3 scripts/findings/findings.py` (`FINDINGS` below);
never edit `findings/` by hand.

## 1. Collect the queue

Runs are the directories under `findings/` (in the main checkout). Use
the run id argument if given, else every run.

For each run, call `FINDINGS list <run> --state blocked --state pitched`
(add `--state deferred` with `--deferred`). Read each blocked finding
with `FINDINGS show <id>`: it has an **open ask** when its last decision
has an `ask`. Order the queue:

1. open asks (a run stopped to ask; findings they name are held);
2. other blocked findings (a run failed for a reason that needs no
   choice);
3. the rest by score, highest first.

If the queue is empty, say so and go to step 4.

## 2. Ask about each finding

Read `FINDINGS show <id>` and print, in plain words:

1. Title, id, category, severity, confidence, effort.
2. The issue, then the claim's evidence in a line or two.
3. For a blocked finding: the current decision (candidate and note),
   any earlier answers, and the open ask or the block reason.
4. Candidates, best design first. Mark the ideal and the recommended
   one. For each: summary, trade-offs, worst case.
5. Rejected solutions and why each lost.
6. The recommendation and its justification.
7. Dependencies (`depends_on`) and their states.
8. The critic's verdict and points, and whether the pitch was revised
   after it. A pitch with no critique is marked **not critiqued**
   (`plan/2026-10-21-1424/I15`); say so plainly.

If the user says a finding must land after another, record it with
`FINDINGS depends <id> --add <other id>` (`--remove` drops one); the
drain will not start it until the other merges.

Then ask with `AskUserQuestion` (one question per finding).

### An open ask

Print the ask: what the run found, its options with trade-offs, its
recommendation, and the findings it affects. Offer one option per ask
option (recommended first, "(Recommended)"), plus `Decide again` when
the 4-option limit allows. Map the answer:

- An ask option, or "Other" text that keeps the decision:
  `FINDINGS answer <id> --note "<option key>: <option summary>. <user's text>"`.
  The decision and branch stay; the next run resumes the work.
- `Decide again`: ask a follow-up question over the pitch's candidates,
  as for a pitched finding, and record it with `decide` (a new decision
  and a fresh branch).

### A blocked finding without an ask

Offer `Resume` (recommended when the block reason says the work itself
was fine), `Decide again`, and `Drop`. `Resume` asks for a short
instruction and records it with `FINDINGS answer <id> --note "<text>"`.

### A pitched or deferred finding

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
it. Record each decision as it is made; never start work here.

## 3. Report

End with a short table: id, title, and what the user chose (candidate,
answer, defer, drop).

## 4. Offer a drain

Call `FINDINGS queue` and say how many findings are runnable, waiting
on dependencies, held by an open ask, asking, and blocked. If any are
runnable, ask with `AskUserQuestion` whether to start one `drain` run
now (implements the runnable findings, at most 3 at a time, in
dependency order) or leave the queue for later. On yes, start it in the
background and do not wait:

```
Workflow({name: "drain"})
```

If the name is not found (workflows register on reload), use
`Workflow({scriptPath: "<main checkout>/.claude/workflows/drain.js"})`.

This is the only workflow this skill starts. Never start a second drain
while one is running (`/workflows` shows it): two drains can run one
finding twice and waste the work.

Results land on `master` as commits with a `Finding: <id>` trailer. A
run that needs the user stops its finding with an ask or a block, sends
a notification, and the finding comes back in the next `/pick-fix`.
