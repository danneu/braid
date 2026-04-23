---
name: stdout/stderr routing contracts need a CLI-level test
description: Unit tests on a render helper cannot observe stream routing; when a fix changes stdout-vs-stderr behavior, a focused VM/CLI subtest that captures `>stdout 2>stderr` is mandatory
type: feedback
originSessionId: c04a4fe4-f8c2-42dd-a3df-3cbefe36be80
---
If a fix changes which stream output lands on (stdout vs stderr), unit
tests on the render helper cannot catch a regression -- they only see the
returned string. The call site could switch from `print!` back to
`eprintln!` and every unit test would still pass.

When a plan introduces a stream-routing contract (e.g. "dry-run output is
redirectable with `> preview.txt`"), it must be paired with a CLI-boundary
test that captures the two streams separately:

```sh
braid lock --dry-run >/tmp/stdout 2>/tmp/stderr
```

and asserts the expected split. In braid, that means a subtest in the
relevant `tests/cli/<cmd>.py` VM test file (these files are already
registered in `flake.nix` under `checks`, so no new flake wiring is
needed unless a new file is created).

**Why:** Dan flagged this as a Medium finding in plan review -- "the new
`braid lock --dry-run > preview.txt` contract can regress silently;
changing the call site back to stderr or re-splitting streams would still
pass every proposed test."

**How to apply:** Any plan that touches `print!` vs `eprintln!` for
user-facing output, or claims a stream-routing property, must include a
CLI-level subtest that redirects both streams and asserts on each. The
shortest deterministic preview (e.g. "nothing to do.\n") is enough -- the
test is pinning routing, not content.
