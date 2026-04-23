---
name: Prefer CLI VM regression test over pure formatter helper
description: When fixing a user-visible CLI output/control-flow bug, add a behavioral VM test that drives the real command; don't extract a single-use formatter helper just to make the tests cheap
type: feedback
originSessionId: 59fde6fc-0396-43cd-98d2-4afe82145554
---
When a bug is a user-visible control-flow issue in a CLI command (e.g. wrong
stderr line printed on a failure path), don't extract a pure formatter
helper and test its string output. That produces structure-sensitive tests
that mainly prove the helper exists; they don't prove `braid <cmd>` emits
the right output on the real failure path, and the helper becomes a
single-use abstraction that exists only to satisfy brittle tests.

**Why:** Braid already treats best-effort `cryptsetup close` messaging as
behavior worth CLI VM coverage -- see `tests/cli/braid-remove-disk-busy.py`,
which holds the mapper busy via `losetup`, runs the real command, asserts
on stderr, exit status, and post-state (mapper still open, btrfs state
correct). That pattern is reusable for any "close fails -- warn but don't
fail the command" contract.

**How to apply:** For bugs in this class (output correctness on a rare
command failure path), prefer to:
1. Fix the control-flow directly in the command module.
2. Add a CLI VM test modeled on the existing busy-close pattern: force the
   failure via `losetup` (or equivalent), run the real command, assert the
   warning is present AND the contradictory success line is absent AND the
   command exits 0 AND post-state is correct.
3. Skip the pure-formatter helper. One call site + no helper is simpler
   and the VM test actually binds to user-visible behavior.

Extract a formatter helper only when there are multiple call sites or the
formatting logic is complex enough that unit-level testing of the string
output adds real value beyond what the VM test provides.
