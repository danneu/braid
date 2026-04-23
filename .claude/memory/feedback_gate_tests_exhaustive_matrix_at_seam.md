---
name: Gate regressions need exhaustive matrix coverage at a unified seam
description: When a cmd-level change adds a gate with N axes, tests must (a) route BOTH gate branches through the same seam and (b) cover every matrix cell whose output differs from a plausible wrong gate -- not just one positive + one negative
type: feedback
originSessionId: 5fe2df90-4f36-4b3b-b39f-b4b1398c9f9d
---
When a `cmd_*` change introduces a gate like `foo && bar`, designing
the tests correctly requires two disciplines that are both easy to
miss:

**1. Unified seam across branches.** If the test seam (scripted
reader, recording runner, etc.) is wired in only on the "positive"
branch, the "negative" branch isn't observable -- and any regression
that over/under-triggers the gate hides from tests. Concretely: when
threading a dependency-injected seam like `passphrase_reader: &dyn
PassphraseReader` through `AddParams`, the `cmd_*` function must call
the seam-aware helper on BOTH sides of the gate (pass a flag rather
than branching the call site).

**2. Matrix coverage, not sample coverage.** For a gate with input
matrix cells (e.g. `(any_needs_format, live_target_present)`), write
one cmd-level test per cell whose output the gate controls. Do NOT
settle for "one typo-rejects + one happy-path" if the rejected
alternate gate formulations (e.g. `membership.is_empty()`,
`confirm_new = any_needs_format`) would still satisfy those two tests.
The matrix cells that distinguish the good gate from plausible wrong
ones are the ones to pin.

**Sanity check the tests by reverting the gate.** Before shipping,
temporarily replace the gate with each plausible wrong formulation
(`false`, `any_needs_format` alone, `membership.is_empty()`) and
confirm each breaks at least one test. If a rewrite of the gate
passes all tests, the matrix is underspecified.

**Why:** Plan-review cycle for braid's bootstrap-confirm gate: first
pass had tests only on the bootstrap (positive) branch via the seam;
the non-bootstrap (negative) branch couldn't be observed. Second pass
covered `(true, false)` + `(false, false)` cells but the rejected
`membership.is_empty()` gate would have passed both, because neither
cell distinguished it. Tests became load-bearing only once they covered
`(true, false, membership-empty)` + `(true, false, membership-present)`
+ `(true, true)` -- the cells where each wrong gate gives a different
answer than the right one.

**How to apply:** Any time a `cmd_*` change adds a boolean gate
(confirm/skip/early-exit) derived from multiple inputs, plan the
regression tests as a matrix first. For each plausible wrong
formulation of the gate, identify which matrix cell would expose it,
and make sure the test suite hits that cell. If the seam is in
`Params`, wire every cmd branch through it -- don't branch at the
call site.
