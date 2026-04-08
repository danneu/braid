---
name: Test at the layer the bug actually breaks
description: When fixing a bug, the primary test must exercise the layer where production fails — not a tangential canary that happens to touch the same code
type: feedback
---

When a bug is "wiring breaks at layer X causing production behavior Y to regress," the primary test must catch X→Y end-to-end. Don't substitute a parser/unit canary that only proves "the layer past X works fine when fed the right input." A canary that doesn't fail when the bug is reintroduced is not a regression test for that bug.

**Why:** During plan review, I proposed a golden fixture parser test as the "parser repro" for a `btrfs replace status` cmd-helper bug (missing `-1` flag making `braid idle` hang). The user pointed out the golden test only proves the parser accepts real text — it does **not** fail if `cli/src/cmd.rs` drops `-1` again. The actual production failure mode (idle/progress/recovery blocking) was relegated to "out of scope, optional" in the same plan. Tests can pass while the regression returns.

**How to apply:** Before marking a plan complete, mentally re-introduce the bug and ask: "does any new test in this plan FAIL?" If the answer is "no, but it makes drift more visible," the plan needs an end-to-end assertion at the failure layer. For wiring/cmd-helper/integration bugs, prefer extending an existing VM test (lower marginal cost) over building net-new infrastructure. For braid specifically: `tests/cli/replace-inhibits-suspend.py` and `tests/cli/braid-idle.py` already cover replace + idle paths and are good extension points.
