---
name: Test preamble block comments are literal /* ... */
description: Per AGENTS.md, every test starts with a `/* ... */` block comment containing Intent / Why it exists / Scenario -- not `//` line comments, despite many existing tests using `//`
type: feedback
originSessionId: 48f96c6f-8cc3-41f0-843b-f64aaf91f56c
---
When writing or planning new Rust tests in this repo, the preamble must be a
real `/* ... */` block comment with three explicitly labeled sections:

```rust
/*
 * Intent: one-line statement of the behavior verified.
 * Why it exists: the regression risk this protects against, ideally with
 *   reference to the incident or commit that prompted it.
 * Scenario: the concrete real-world sequence the test models.
 */
#[test]
fn the_test() { ... }
```

**Why:** AGENTS.md explicitly says "block comment" for the required
preamble. Many existing tests in the repo use `//` line comments for their
preambles, but Dan enforces the literal `/* ... */` form in reviews.
Submitting `//` preambles in a plan or PR invites avoidable review churn.

**How to apply:** For any new `#[test]` added in a plan or directly in code,
default to `/* ... */` block comments. Do not copy the `//` style from
neighboring existing tests -- they are grandfathered but not the standard.
The existing example to model on is
`lock_retries_busy_close_then_succeeds` in `cli/src/lock.rs:1319-1337`.
