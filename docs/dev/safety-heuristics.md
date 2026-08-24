---
intent: Implementation heuristics for mutating-command code -- invariant placement, fail-closed policy, hard-vs-debug_assert residual guards, and state-enum discipline. Read before touching mutation code.
---

# Mutation safety heuristics

These elaborate [principle 3, safe-by-construction operations](../design/principles.md#3-safe-by-construction-operations) for contributors.

- Query the authoritative source of state directly; do not pre-gate it with a
  cheaper but weaker observable such as path existence.
- Put invariant checks at the layer that owns the invariant. Primitive-level
  checks belong inside the helper that performs the unsafe operation; caller
  policy gates belong at callsites.
- Keep diagnostic refinements out of mutating-command state enums when the new
  distinction only matters for `status`, `doctor`, TUI, or error rendering.
- Classify journal-bearing failures once at the command boundary by loading the
  journal through `journal::load_journal`. Do not infer recovery advice from
  path existence or from a lifecycle phase threaded through lower-level
  operations: a valid visible journal means recovery can be retried, an absent
  journal after an installed write means deletion durability is uncertain, and
  an unreadable or unparseable journal requires storage or manual repair before
  recovery can consume it.
- Keep the final journal clear as the last fallible state transition. Cleanup
  after clear may be best-effort and non-fatal, but a later propagated failure
  would make an absent journal ambiguous with an unconfirmed durable deletion.
- Set fail-closed policy from the downstream failure mode. If a branch can
  corrupt state or strand a journal when a preflight is wrong, every uncertainty
  in that branch is a hard error even if a sibling branch can warn and proceed.
- Residual invariant checks must be hard errors in all builds; do not replace a
  production guard with `debug_assert!`.
- Split post-commit failure variants by the operator's remediation and on-disk
  consequence, not by implementation layer.
