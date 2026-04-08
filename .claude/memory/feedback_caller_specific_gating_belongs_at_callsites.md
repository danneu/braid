---
name: caller-specific gating belongs at callsites, not in shared helpers
description: When a "should we do X now?" rule depends on caller context, don't bake it into a shared helper that takes the context as a parameter — make the helper pure and let each caller gate at its own callsite
type: feedback
---

When refactoring shared helpers, do NOT encode caller-specific control-flow rules into the helper, even if the rule "looks general." If two callers have different reasons to invoke the helper, a single rule will be wrong for at least one of them.

**Why:** I drafted a `resolve_credential(plan, source)` helper in plans/wip/playful-wobbling-duckling.md that returned `None` when `plan.to_unlock.is_empty()`. This was correct for `cmd_unlock` (don't prompt when every mapper is already open) but silently regressed `cmd_recover`, which always reads the passphrase upfront because its post-mount relock cycle closes every mapper and must reopen them — even if the initial plan had nothing to unlock. The bug was a real regression in a load-bearing recovery path, not a hypothetical. The reviewer caught it and pointed at `cli/src/recover.rs:222` and `:340` as the relevant call sites.

**How to apply:** When proposing a helper that takes both a *context* (plan, state, config) AND a *source*, ask: "is the helper's gating rule the SAME for every caller?" If not, the helper should be a pure resolver and the gating belongs at each callsite. Pure helpers + caller-side gates make differences visible in local control flow; baked-in rules hide them. Specifically for braid: any future "should we read X now?" decision involving `cmd_unlock` and `cmd_recover` will likely diverge because recover has the post-mount relock cycle as a hidden dependency on later state.

**Related:** This pairs with the existing `feedback_invariants_at_right_layer.md` rule but in the opposite direction — that one says "put guards at the layer that owns the invariant"; this one says "if no single layer owns the gating decision, don't pretend one does."
