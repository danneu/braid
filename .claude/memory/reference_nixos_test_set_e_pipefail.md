---
name: NixOS test driver runs commands under set -euo pipefail
description: Every machine.succeed/execute command in NixOS VM tests is wrapped with `set -euo pipefail`, so any non-zero exit in a chain aborts before later statements run
type: reference
---

The NixOS test driver auto-prepends `set -euo pipefail` to every shell command before sending it to the VM (`test_driver/machine/__init__.py` around line 858). This is invisible from the test script but has real consequences for chained commands.

**Symptom:** A chain like `... ; wait $pid_loser ; echo $? > /tmp/exit-a ; ...` silently aborts when `wait` returns non-zero. The exit-code file is never written, and the next subtest assertion fails with `cat: /tmp/exit-a: No such file or directory` — pointing at the wrong layer.

**Idiom for capturing a non-zero exit without aborting:**

```sh
ec_a=0 ; wait $pid_a || ec_a=$? ; echo $ec_a > /tmp/exit-a
```

The `||` consumes the non-zero into the variable, so errexit does not fire. Works for any command whose non-zero exit is expected (`wait`, `grep`, `diff`, etc.).

**Where this matters most:** concurrent-process tests where one process is expected to exit non-zero (e.g., fail-fast lock contention, expected error paths). The blocking-flock world hid this because everything succeeded; flipping to non-blocking exposed it.
