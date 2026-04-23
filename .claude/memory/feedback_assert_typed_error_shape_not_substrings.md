---
name: Assert on typed error shape, not message substrings
description: Propagation tests should match the typed error variant and payload; only lock user-facing strings in a separate Display-targeted test if that's what the test is for
type: feedback
originSessionId: c7a096e0-6eca-4482-b8dc-79884364f1a5
---
When writing a caller-boundary test that verifies an error propagates correctly, assert on the typed error shape (variant + payload), not on message substrings.

**Why:** Dan rejected a plan that proposed asserting `err.to_string().contains("mapper")` to prove `ProbeError::MapperConflict` surfaces through `build_status_report`. His feedback: "that makes the test brittle to wording changes while still being weaker than it could be about the thing that matters: that `StatusError` wraps `ProbeError::MapperConflict` with the expected payload." Substring matches fail on harmless message wording tweaks and still don't prove the right variant traveled end-to-end.

**How to apply:** For propagation tests, use `match` on the typed error:

```rust
match result {
    Err(StatusError::Probe(ProbeError::MapperConflict { expected, found, .. })) => {
        assert_eq!(expected, ...);
        assert_eq!(found, ...);
    }
    other => panic!("expected StatusError::Probe(MapperConflict), got: {other:?}"),
}
```

If the test's goal is specifically to lock user-facing wording (e.g. a remediation hint), put that in a separate `Display`-targeted unit test against the source error type -- don't conflate propagation coverage with string-wording coverage.

**Companion rule for Display-locking tests:** when the test IS the Display-targeted one (protecting a message-construction helper from refactor drift), use `assert_eq!(err.to_string(), "...full expected bytes...")`, not `err.to_string().contains("braid-aaa") && .contains("exit 5")`. Substring checks leave gaps: a refactor that drops `.trim()`, swaps two locals, changes punctuation, or reflows the shape can still pass every substring assertion while changing what the user sees. Full-string equality catches all of those in one line. Reviewer pushback from 2026-04-23: "The proposed assertions only check substrings plus trim/casing, not the complete rendered message ... make the new test assert the exact `err.to_string()`."
