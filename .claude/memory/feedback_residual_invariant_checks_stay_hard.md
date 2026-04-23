---
name: Residual invariant checks must be symmetric and fail closed in all builds
description: When splitting a bidirectional runtime validation into type-encoded entry points, every entry point keeps a hard runtime check on whichever invariant axes the type system doesn't kill -- and the checks are hard errors in all builds, never debug_assert!.
type: feedback
originSessionId: 8bdd39e5-0755-4469-8848-329302dcf2c2
---

When a refactor moves a bidirectional `match (a, b)` runtime validation into split entry points (each encoding one case via the type system), audit *every* resulting entry point for residual preconditions the types still don't cover, and give each one a hard runtime check. Two failure modes to avoid:

1. **Asymmetric split.** The original validation guarded both directions; the split often only type-encodes one. Example: splitting on a credential-optional axis kills "credential missing when required" because the argument vanishes, but the "plan shape" axis (e.g. `plan.to_unlock` emptiness) still flows through both entry points as data. The original had two arms; the split naturally leaves one arm covered by types and the other needing an explicit `if`. Forgetting to add the symmetric check in the second entry point halves the guarantee.
2. **`debug_assert!` instead of hard `Err(...)`.** A `debug_assert!` that silently falls through in release builds turns a caller-wiring regression into a production-successful operation. That defeats the whole point of the split: the invariant was supposed to be strengthened, not weakened. The residual check exists specifically to catch the regression the type system couldn't catch alone, so it must fire where that regression would actually happen (prod).

**Why:** The refactor's value proposition is "make invalid states unrepresentable." If one axis stays representable, the invariant has to be enforced somewhere, and it must be somewhere that fires in all builds. Otherwise the refactor has made the invariant *weaker* than the runtime-match it replaced, even though the signature looks tighter.

**How to apply:** Any time a plan splits a runtime `match` into multiple entry points, list the axes the original validated, tick off which axes the type system now enforces per entry point, and require a hard `Err(...)` check for every axis still representable on that entry point. Pin each check with a direct boundary test that calls the production function (not a test helper that may also dispatch). Either keep hard checks or go all the way to a newtype constructor that makes the invalid shape unconstructable -- never replace a hard check with `debug_assert!`.
