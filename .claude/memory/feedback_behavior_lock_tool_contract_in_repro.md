---
name: behavior-lock tool contract assumptions in repro/VM tests
description: When code trusts a specific tool behavior (exit code, output format), add assertions to a live-tool repro/VM test, not just mocked unit tests
type: feedback
originSessionId: e4c2feef-b6b5-481b-a648-f88718239625
---
When braid code is changed to depend on a specific external-tool behavior -- a particular exit code, a particular output wording, a particular return-value path -- the unit tests prove the *classifier* is correct given the assumed behavior, but they do NOT prove the tool still behaves that way. Extend an existing repro/VM test (or add one) to assert the exact tool contract the production code now depends on, and list it in the plan's verification section as a required gate.

**Why:** I pivoted `close_mapper_with_retry` to classify `cryptsetup close` busy by exit status 5 and non-busy by exit status 4. Plan v2 included unit tests with hand-written MockRunner fixtures using those codes, but nothing in the repo would have caught a nixpkgs bump that changed cryptsetup's exit-code contract -- every mocked test would still pass while live `braid lock` silently misclassified. User pushed back: extend `tests/repro/cryptsetup-close-mounted.py` to assert `exit_code == 5` for busy close and `exit_code == 4` for close-after-already-closed. That behavior-locks the assumption the unit tests depend on.

**How to apply:** Any plan that writes `== <code>` or `.contains("<wording>")` as a classifier against an external tool must identify (or add) a live-tool test that asserts the same code/wording directly. If the live-tool test would be non-trivial to add, pause and reconsider whether the classifier is actually robust. Mocked unit tests alone are insufficient -- they verify the mapping from assumed behavior to classifier, not the assumption itself.
