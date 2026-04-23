---
name: Prefer injected sleeper over cfg-gated const for retry-delay tests
description: When removing a thread::sleep wall-time tax from unit tests, inject the sleeper as a dependency rather than `#[cfg(test)]` gating the Duration constant
type: feedback
originSessionId: 48f96c6f-8cc3-41f0-843b-f64aaf91f56c
---
When optimizing unit-test wall time by removing a `thread::sleep(CONST)` cost
from a retry loop, do NOT reach for `#[cfg(test)] const X = 0ms`. Prefer
injecting a `Sleeper` trait (production impl calls `thread::sleep`, test impl
records durations without sleeping).

**Why:** A `cfg(test)` split causes test-vs-prod build divergence: no single
test binary ever exercises the production timing value, so there is no
deterministic unit test that can verify "prod sleeps N ms between attempts."
If the sleep is accidentally removed or the constant gets wrong-pathed, only
a race-dependent VM test will notice -- and race-dependent tests do not
provide deterministic coverage of timing code.

An injected sleeper lets one unit test drive the retry path with a
`RecordingSleeper` and assert `recorded_durations == vec![CLOSE_RETRY_DELAY;
N-1]` plus `CLOSE_RETRY_DELAY == Duration::from_millis(500)`. That test runs
in microseconds and locks prod behavior deterministically. All other tests
use a `NoopSleeper` and also pay zero wall time.

**How to apply:** Any time a review-plan optimizes test speed by zeroing out
a timing constant, pivot to dependency injection if (a) the constant's value
is part of the behavioral contract (not an arbitrary implementation detail)
and (b) no other deterministic test pins it. Watch for plans that cite
race-dependent or flakiness-prone VM tests as "deterministic coverage" --
that claim is usually wrong.
