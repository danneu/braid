# braid-pool-lock-fd-gentle-lantern

## Context

`modules/braid/braid-wrapper.sh:40` opens fd 9 on `/run/braid-pool.lock`
and acquires an advisory `flock` for `unlock|add|recover`. fd 9 has no
`FD_CLOEXEC`. Line 64 (`@braidBin@ "$@"`) is a normal command
invocation, not the shell `exec` builtin: bash forks a child and
execs the braid binary in that child, so the wrapper bash stays alive
holding fd 9 while braid (and every descendant it spawns) inherits
fd 9 into the forked child.

The braid binary spawns one long-lived helper for the duration of
mutating operations: `systemd-inhibit` from
`cli/src/inhibit.rs:131-137`, deliberately placed in its own process
group via `process_group(0)` so signals to braid's foreground pgid don't
reach it. systemd-inhibit in turn supervises a
`sh -c 'printf READY; exec sleep infinity'` child.

When braid dies without running `Drop` -- SIGKILL, OOM-kill (also
SIGKILL), default-disposition signal termination (SIGINT today,
because braid has no Rust signal handler installed; SIGTERM, SIGHUP,
etc.), or an aborting panic (`panic = "abort"`, explicit
`std::process::abort()`) -- the `SleepInhibitor::drop` path that would
`kill(-pgid, SIGKILL)` never fires. (Ordinary unwinding panics still
run `Drop` and are not in scope here.) The systemd-inhibit subtree
survives, gets reparented to PID 1, and keeps the inherited fd 9
open. flock is held on the open file description, so the lock stays
held until that fd is closed by hand.

Captured in the wild after Ctrl-C'ing `braid add` mid-balance: an
orphan `systemd-inhibit ... sh -c 'printf READY; exec sleep infinity'`
with `PPID=1` and `ELAPSED=31:00` was still holding fd 9 on
`/run/braid-pool.lock`. The next `braid recover` printed
`another braid operation is already in progress (...) retry once it
finishes` -- but no operation was running. Releasing the lock required
manually killing the orphan.

This is independent of two related bugs being tracked separately:

- pool.json deferred past `btrfs device add` (separate plan).
- SIGINT -> balance pause / no Rust signal handler (separate plan).
  Note: a clean SIGINT handler would let `Drop` run for SIGINT, which
  would *partially* mask this bug for that one signal. SIGKILL / OOM /
  panic would still leak the lock. This bug must be fixed
  independently.

Audit (Explore agent, this session) confirmed `cli/src/inhibit.rs` is
the only orphan-prone subprocess in `cli/src/`; no other callsite uses
`process_group(0)`, `setsid`, or daemonization patterns.

## The fix

One-line change in `modules/braid/braid-wrapper.sh`. Close fd 9 in the
forked child that exec's the braid binary, while the wrapper itself
keeps fd 9 open for the duration of the operation:

```bash
# Replace line 64
@braidBin@ "$@"

# With
@braidBin@ "$@" 9>&-
```

`cmd 9>&-` applies the redirection in the forked child before exec, so
braid (and every descendant including systemd-inhibit and its
`sh + sleep` subtree) starts with no fd 9. The wrapper bash retains its
own fd 9, so the lock is held for the entire span between flock
acquisition and the wrapper's `exit $ret` on line 97. When the wrapper
exits -- success, failure, or cascading death because the braid binary
was killed -- its fd 9 closes and the lock is released by the kernel.

`9>&-` is a no-op on subcommands that never opened fd 9 (the wrapper
only opens it for `unlock|add|recover` when not `skip_fixup`); bash
treats closing an unopened fd as a silent success. Capturing `ret=$?`
on line 65 is unchanged. Post-processing on lines 67-96 still runs
inside the wrapper with fd 9 still held (irrelevant for those steps,
but consistent).

This was preferred over `flock -o /run/braid-pool.lock @braidBin@`
(the alternative considered in the bug report) because the wrapper's
existing structure -- pre-stop logic for `lock`, post-success block
for `unlock|add|recover`, conditional skip_fixup gating -- has to keep
running in bash, and `flock ... exec` would replace the wrapper. The
one-character redirect is the minimal diff that preserves all of that.

## Why this is safe

- Lock semantics unchanged. Wrapper still acquires the lock before
  invoking braid, still releases it on its own exit. Only the
  inheritance chain is cut.
- braid never reads or writes fd 9 (it's purely a wrapper-side token);
  removing it from braid's environment cannot break behavior.
- Wrapper death cascades release the lock for free, including the
  Ctrl-C case where the wrapper bash dies in the foreground pgid.
- Subcommands that don't open fd 9 (`status`, `tui`, `lock`,
  `replace`, `remove`, etc.) are unaffected -- `9>&-` is a silent
  no-op when fd 9 isn't open.
- `skip_fixup` path unchanged: when fd 9 was never opened, the
  redirect is a no-op.

## Out of scope (carried over from bug report)

- **Other lock-needing subcommands.** The Explore agent confirmed
  `remove`, `remove-missing`, `replace`, and `enroll` all mutate
  `pool.json` and/or LUKS keyslots but the wrapper does not lock for
  them today. The bug report explicitly defers this audit; do not
  expand the wrapper's case statement as part of this fix. Worth
  filing as a follow-up.
- **Moving lock acquisition into the braid binary.** Cleaner
  long-term (Rust can set `O_CLOEXEC` and couple lock lifetime to
  `SleepInhibitor`), but a much bigger change with no behavioral
  benefit beyond what the wrapper fix already gives.

## Files in scope

- `modules/braid/braid-wrapper.sh` -- the fix (one-character diff on
  line 64).
- `tests/module/wrapper-pool-lock-not-inherited.nix` and `.py` -- new
  structural test (see below).
- `tests/module/wrapper-pool-lock-released-after-sigkill.nix` and
  `.py` -- new regression test (see below).
- `flake.nix` -- register both new VM tests under `checksFor`. Both
  must be passed `linuxCrane.braid-cli-unwrapped` (NOT
  `linuxCrane.braid`), because the module wrapper does its own
  PATH/wrapper handling on top of the unwrapped binary. Mirror the
  existing `pool-lock-contention` entry around line 541:

  ```nix
  wrapper-pool-lock-released-after-sigkill = pkgs.testers.nixosTest (
    import ./tests/module/wrapper-pool-lock-released-after-sigkill.nix {
      braid = linuxCrane.braid-cli-unwrapped;
    }
  );
  wrapper-pool-lock-not-inherited = pkgs.testers.nixosTest (
    import ./tests/module/wrapper-pool-lock-not-inherited.nix {
      braid = linuxCrane.braid-cli-unwrapped;
    }
  );
  ```

  This is the critical wiring step. The `tests/cli/` family
  (`add-inhibits-suspend.nix`, etc.) puts `braid` directly into
  `environment.systemPackages` -- that path resolves to the
  flake-level `linuxCrane.braid` `makeWrapper`-only package and
  never touches `modules/braid/braid-wrapper.sh`. With that wiring,
  the regression test would NOT fail when `9>&-` is removed (because
  fd 9 is never opened in the first place), and the structural test
  would not find `/proc/<wrapper_pid>/fd/9` to assert on. The
  module-import path used by `pool-lock-contention.nix` and
  `systemd-lifecycle.nix` is the only correct shape for these tests.

No `docs/decisions/` ADR. The wrapper docstring on lines 31-36
already explains the lock invariant; this fix is a bug correction, not
an architectural shift.

## Tests to add

Both tests must follow the AGENTS.md preamble convention: a literal
`/* */`-style block comment is for Rust, but Python tests in `tests/`
use the `# Test: <name>` + Intent/Why/Scenario block at the top of
`.py` files (see `tests/module/pool-lock-contention.py:1-14` and
`tests/cli/replace-inhibits-suspend.py` for the canonical shape).

Both must be registered in `flake.nix` `checksFor` -- the harness
dispatches on `checks` entries, not on filenames in `tests/`.

### 1. `wrapper-pool-lock-released-after-sigkill` (primary regression)

The behavior-locking test: this MUST fail when the bug is
reintroduced (i.e. when `9>&-` is removed from the wrapper).

`.nix` shape: copy the module-import wiring from
`tests/module/pool-lock-contention.nix:25-36` (`imports = [
../../modules/braid ... ]; braid = { enable = true; package = braid;
};`) so `braid` on PATH resolves to the module wrapper script. Test
script can mirror `tests/cli/add-inhibits-suspend.py` for the
bootstrap/payload/readiness sequence, but the .nix wiring MUST come
from the `tests/module/` family.

Shape:

- Bootstrap with `braid add disk1=...` (1-disk pool), as in
  `add-inhibits-suspend.py` Phase 1. Don't try to seed via the
  `initrd-fixture` path -- using real `braid add` exercises the
  wrapper's fd-9 acquisition for free and avoids divergence from
  the command's actual mutation window. (The `pool-lock-contention`
  test seeds `pool.json` by hand because it only needs `braid
  unlock` to reach the wrapper; we want the longer mutation window
  that `braid add` gives us.)
- Write a 400 MiB urandom payload (Phase 2 of `add-inhibits-suspend`)
  so the second add has real `pool_balance_raid1` work to do.
  Without the payload the balance window is too short to land the
  SIGKILL inside the bug-trigger window, and the test races on
  fixture timing rather than on the fix.
- Background `braid add disk2=...` so `$!` is the module wrapper
  bash unambiguously. AVOID the `printf '%s\n' $pq | braid add ...
  --passphrase-stdin` pipeline shape from `add-inhibits-suspend.py`
  -- with a pipe, `$!` is the rightmost pipeline element's PID and
  the lifecycle of the printf side adds noise. Two clean options:
    1. If `braid add` accepts `--passphrase-file` (verify against
       the current clap definition before relying on this), write
       the passphrase to `/tmp/passphrase` and run
       `nohup braid add disk2=... --passphrase-file /tmp/passphrase
       --yes >/tmp/add.log 2>&1 & echo $!`.
    2. If only `--passphrase-stdin` exists, write the passphrase to
       a temp file and use input redirection (NOT a pipe) on the
       backgrounded command:
       `nohup braid add disk2=... --passphrase-stdin --yes
       </tmp/passphrase >/tmp/add.log 2>&1 & echo $!`.
       Redirection is applied to braid directly, so `$!` is braid's
       (i.e. the module wrapper bash's) PID.
  Capture via `wrapper_pid = machine.succeed(...).strip()`, the
  same shape as `pool-lock-contention.py:42-47`.
- Wait on TWO independent readiness signals before killing:
  1. `find_braid_sleep_inhibitor(list_inhibitors())` returns non-None
     (the inhibitor seam fired). Use the helpers from
     `tests/cli/inhibitor_helpers.py`, concatenated via the same
     `builtins.readFile` pattern as
     `tests/cli/replace-inhibits-suspend.nix:54-60`.
  2. EITHER `/var/lib/braid/pending-op.json` exists (journal written
     -- braid is past the irreversible boundary) OR
     `/sys/fs/btrfs/*/exclusive_operation` reports `balance` (kernel
     is in the long-running phase). Polling for `pending-op.json` is
     enough by itself; the `exclusive_operation` check is the
     stronger signal that we're inside the balance window where the
     bug is most easily reproduced.

  The double signal matters because `cmd_add` acquires the inhibitor
  BEFORE `journal::write_journal` (`cli/src/add.rs:492` vs
  `cli/src/add.rs:520`). An inhibitor-only signal can leave the test
  killing braid before the journal is written, after which subsequent
  `braid` invocations would refuse on journal-presence grounds and
  the test would fail for journal reasons rather than fd inheritance.

- Find the braid binary PID:
  `pgrep -P <wrapper_pid> braid`. SIGKILL it: `kill -9 <braid_pid>`.
  Killing the binary (not the wrapper) is what reproduces the bug:
  the wrapper bash drops its own fd 9 cleanly on exit, but the
  orphaned `systemd-inhibit` keeps fd 9 alive.
- Wait up to 5s wall-clock for `find /proc/*/fd -lname
  '*braid-pool.lock' 2>/dev/null` to return empty
  (`wait_until_succeeds`-style poll, as in
  `pool-lock-contention.py`). Assert empty. With the bug, this list
  is non-empty (the orphan's fd 9). With the fix, it is empty
  immediately after the wrapper exits.
- Acquirability assertion: `flock -n /run/braid-pool.lock true` --
  exit 0 means the lock is actually acquirable. This is the direct
  kernel-level check; do NOT use `braid recover` here, because
  `braid recover` can fail for unrelated reasons (no journal
  present, recovery logic refusing on the half-written state) and
  would mask whether the lock was the problem.
- Optional: as a *separate* smoke subtest, after confirming
  `pending-op.json` exists, run `braid recover` and assert exit 0.
  This is end-to-end coverage that the post-fix system can actually
  resume from a SIGKILL'd `braid add`. Keep it segmented so a
  recover-side failure doesn't masquerade as a fd-leak failure.

This test FAILS pre-fix (orphan inhibitor pins fd 9 -> flock -n
fails with EWOULDBLOCK) and PASSES post-fix (wrapper exit releases
the only remaining fd 9).

### 2. `wrapper-pool-lock-not-inherited` (structural, timing-independent)

A defense-in-depth test that catches inheritance regressions even
when the orphan path doesn't fire in a given run. No SIGKILL needed.

Same `.nix` wiring as test #1 (module-import path from
`pool-lock-contention.nix`, `linuxCrane.braid-cli-unwrapped` in the
flake). Same fixture and bootstrap shape as test #1 (1-disk pool +
400 MiB payload + background `braid add disk2`, with the same
no-pipeline PID-capture pattern).

- Wait on the same double signal as test #1 (inhibitor present AND
  pending-op.json or `exclusive_operation=balance`) -- this proves
  both that the wrapper has fd 9 open and that the full subprocess
  tree (braid binary -> systemd-inhibit -> sh -> sleep) exists.
- Capture the wrapper PID (`$!`), the braid binary PID
  (`pgrep -P <wrapper_pid> braid`), and walk descendants by reading
  `/proc/<braid_pid>/task/*/children` recursively, or
  `pgrep -P` walked iteratively.
- Assert `/proc/<wrapper_pid>/fd/9` resolves to
  `/run/braid-pool.lock` (sanity: the wrapper IS the lock holder).
- Assert that for every PID in `{braid_pid} U descendants`, no entry
  under `/proc/<pid>/fd/` is a symlink to `/run/braid-pool.lock`.
- Let the background `braid add` run to completion (`wait_until_succeeds
  "test ! -f /var/lib/braid/pending-op.json"`, as in
  `add-inhibits-suspend.py:165-168`) so cleanup is clean.

With the bug, `/proc/<braid_pid>/fd/9` resolves to
`/run/braid-pool.lock`. With the fix, that fd is absent.

### Tests intentionally not added

- A separate sigkill-during-unlock variant. Same code path as #1, no
  additional coverage. The structural test #2 catches inheritance
  regressions in any subcommand.
- A `--dry-run` skip_fixup test. The fix doesn't touch the
  skip_fixup branch; bash silently no-ops `9>&-` on an unopened fd.
  Adding a test here would be testing bash's behavior, not braid's.
- A concurrent-invocation serialization test. Already covered by
  `tests/module/pool-lock-contention.py`.

## Verification

1. Apply the wrapper fix.
2. `just test-vm wrapper-pool-lock-released-after-sigkill
   wrapper-pool-lock-not-inherited` -- both must pass.
3. Sanity-check the regression assertion: revert the wrapper change
   on a scratch branch, re-run
   `just test-vm wrapper-pool-lock-released-after-sigkill`, and
   confirm it fails. Restore the fix. (This is the "behavior-lock"
   bar: the test must fail when the bug is reintroduced.) If the
   reverted-wrapper run passes, the test is wired to the wrong
   braid -- check the .nix is going through the module wrapper, not
   the flake-level `linuxCrane.braid` package.
4. Run the existing inhibitor / lock tests to confirm no regression:
   `just test-vm pool-lock-contention add-inhibits-suspend
   remove-inhibits-suspend remove-missing-inhibits-suspend
   replace-inhibits-suspend`.
5. End-to-end smoke in a VM: bootstrap a pool, run `braid add`,
   Ctrl-C mid-balance, run `braid recover`, confirm it does not
   print "another braid operation is already in progress".
