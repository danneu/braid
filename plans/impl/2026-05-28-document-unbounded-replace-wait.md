# plan: document the unbounded `Running` arm in `wait_for_kernel_replace_to_finish`

## Context

The `/verify-issue` pass on a recent finding ("`wait_for_kernel_replace_to_finish`
has no overall ceiling and loops indefinitely while the kernel reports
`Running`") concluded the headline claim is wrong: the unbounded loop is a
deliberate correctness barrier, not an oversight.

The rationale lives in commit `491a55d` ("fix(recover): emit heartbeats
during replace wait"):

> Recovery now emits cumulative elapsed-time heartbeats **without adding a
> timeout, preserving the correctness barrier** while showing
> operator-visible liveness.

A timeout would not actually unblock a stuck kernel kthread -- it would
just convert "blocks with heartbeats" into "fails after N hours, re-runs
braid recover, fails after N hours again." Operator visibility is already
covered by the `REPLACE_WAIT_HEARTBEAT_INTERVAL`-cadenced stall lines at
`cli/src/recover.rs:3378-3384`, and SIGINT is the operator escape.

The function's docstring (`cli/src/recover.rs:3287-3301`) currently
explains the two hard-fail modes (`Suspended` / parse-error) and the
warn-and-proceed mode (subprocess error), but says nothing about why
`Running` has no ceiling. That gap is what surfaced the finding -- a
future reviewer (human or AI) reading the loop will keep re-raising it.

The intended outcome is one short docstring addition that names the
design intent so the next reviewer sees it without having to dig through
commit history.

## Change

**File:** `cli/src/recover.rs`

**Edit:** Add a paragraph to the existing `///` doc block on
`wait_for_kernel_replace_to_finish` (currently lines 3287-3301), inserted
after the existing "Two failure modes" paragraph and before the function
signature.

**Content to add (substance, not literal wording):**

- The `Running` arm is intentionally unbounded.
- Two distinct cases to keep straight (so the note doesn't conflate
  them):
  - **Proceeding past `Running` (warn-and-proceed style)** would race
    the resume kthread -- that's the regression class the whole wait
    exists to close. This is why `Running` is not a `[warn]+proceed`
    case like a subprocess error is.
  - **A fail-returning timeout** would *not* race -- it would bail via
    `RecoverError::Failed` (caller at `cli/src/recover.rs:456` uses
    `?`, so the `RemountCycle` action does not execute), and the
    journal would be preserved just as it is for `Suspended` /
    parse-error. The reason such a timeout is still wrong is that it
    adds no remediation: the kernel kthread is unchanged, so the next
    `braid recover` would re-hit the same ceiling.
- Diagnostic surface for a genuinely-stuck kthread is the
  `REPLACE_WAIT_HEARTBEAT_INTERVAL` stall heartbeat (shows cumulative
  elapsed time when `pct` doesn't move), and SIGINT is the operator
  escape.

Target shape: 3-5 lines of doc comment, plain ASCII, matching the
surrounding docstring's tone. Do not add a comment inside the
`ReplaceState::Running` match arm -- the docstring is the right home so
the rationale travels with the function-level contract.

## Reuse

- No new code, no new helpers, no new constants.
- Cross-reference in prose only:
  - `REPLACE_WAIT_HEARTBEAT_INTERVAL` (`cli/src/recover.rs:3285`) for
    the heartbeat cadence the docstring names.
  - The existing principle 13 wording in
    `docs/design/principles.md:73-117` already cites this function as
    the canonical `[wait] -> [warn]+proceed` example; no doc change
    there.

## Out of scope

- No change to `Running` behavior, exit conditions, or any failure
  classification.
- No new tests. The current behavior is already pinned at
  `cli/src/recover.rs:3936-4344` (running-then-finished,
  suspended/parse-error fail-closed, status-error warn-and-proceed,
  heartbeat emission, threshold gating, clock reset). A doc-only edit
  doesn't change any observable behavior, so demanding a new test would
  be structure-sensitive busywork.
- No code change to add a ceiling, watchdog, or stall-failure mode.
- No commit-message-style restatement of `491a55d` -- the doc note
  should stand on its own without citing commit hashes (per the
  `docs(comments)`-style commit `1fe9651`: "drop rust line-number refs
  from comments").

## Verification

- `just test-rust` to confirm the doc-only edit compiles and existing
  `wait_for_kernel_replace_*` tests still pass.
- Eyeball read of `cli/src/recover.rs:3287-3391`: docstring renders as
  a coherent contract, mentions the unbounded `Running` arm, and points
  to heartbeats + SIGINT as the diagnostic/escape surface.
- No VM tests are touched by this change.
