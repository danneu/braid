---
name: Never run cargo fmt autonomously
description: Do not run `cargo fmt`, `rustfmt`, or any formatter-over-source command without an explicit user request, even on the single file being edited.
type: feedback
originSessionId: ccad4c98-ae65-41ad-8449-95d7f6608c91
---
Never run `cargo fmt`, `rustfmt`, or any Rust formatter autonomously. Do not include a formatter step in plans, verification sections, or end-of-task cleanup. Only run a formatter when the user explicitly asks for it in the current turn.

**Why:** Stated directly by Dan on 2026-04-23 -- "i never want agents to run cargo fmt on their own." Context: earlier in the same session I ran `cargo fmt -p braid-cli` after a two-edit refactor. The repo has pre-existing rustfmt drift across most of `cli/src/`, so the formatter rewrote 50+ unrelated files and buried the intended change. Recovery required `git checkout HEAD -- cli/src/` and re-applying the edits by hand. Dan's rule is absolute: don't reach for the formatter even "just on the one file I touched" -- the Edit/Write tool output is already well-formatted, and fmt runs are never the agent's call.

**How to apply:**

- Do not invoke `cargo fmt`, `cargo fmt -p <crate>`, `rustfmt <file>`, or any wrapper (`just fmt`, pre-commit hooks triggered via `git commit`, etc.) on your own initiative.
- Do not list fmt as a verification step in plans.
- If a hand-written edit looks slightly mis-indented, fix it in the Edit call rather than delegating to a formatter.
- Exception: user explicitly asks ("run cargo fmt", "please format this", "apply fmt"). Then scope narrowly to what they asked for.
