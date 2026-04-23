---
name: cmd bug fix regression test must pin the cmd callsite
description: For a bug in a `cmd_*` flow, the primary regression test must drive the real command (asserting typed Err + seam side-effects), not just the extracted helper.
type: feedback
originSessionId: 00ff4d54-57a0-4b0f-843a-72ffed4ef1bb
---
For a bug in a `cmd_*` command flow, the primary regression test must drive
`cmd_*` itself. Helper-level unit tests pass even after someone deletes the
call from the command, so they do not lock in the fix.

**Why:** On the first pass of plan-a-fix-for-mossy-bengio.md (cmd_replace
orphan pool.json bug), I proposed a helper-level test for a new
`validate_old_in_membership` helper. The reviewer rejected it: "testing
`validate_old_in_membership` directly will still pass if someone removes or
bypasses the new `cmd_replace` call-site guard." Supporting helper tests are
fine as fast checks; they are not the regression pin.

**How to apply:** When planning a fix in a `cmd_*` file:
  1. Identify the cmd-level test scaffolding already in the file
     (e.g. `FailingReplaceRunner`, `ReplaceMockFs`, `RecordingInhibitor` in
     cli/src/replace.rs:1404-1472). Model the new test on the nearest existing
     one that asserts both a typed `Err` and seam side-effects (inhibitor
     acquire count, journal presence).
  2. The regression test must assert at least:
     - typed `Err(::Validation(_))` (or equivalent variant) from `cmd_*`
     - expected inhibitor acquire count (usually 0 for a preflight rejection)
     - journal / pool.json / on-disk state matches "no mutation"
  3. Supporting helper-level unit tests are welcome on top, but they do not
     replace the cmd-level test.

Related but distinct from `feedback_test_at_failure_layer.md` (parser-canaries
don't catch wiring bugs) -- this one is about helper-vs-callsite layering
inside a command's own module.
