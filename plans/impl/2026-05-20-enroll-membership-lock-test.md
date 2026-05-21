# Plan: make enroll's pool-lock-precedes-state-read protection explicit

## Context

`tests/module/pool-lock-precedes-state-read.py` exists to pin the invariant
that every Rust-locked dispatch arm acquires `/run/braid-pool.lock` before
reading config, membership, pending journals, probes, or passphrases. For
seven of the eight locked mutators in subtest 1, that invariant is held
to by `--config /nonexistent/braid.json` plus a forbidden-substring list
of config-shaped error strings (`failed to read config file`, etc.) -- a
specific, regression-pinning signal.

Enroll is the exception. Its lock discipline is declared centrally
via `lock_policy(&Commands::EnrollKeyFile)` at
`cli/src/main.rs:140-146` (returns `NonBlocking` when not in
dry-run), and the lock is acquired at the top of `main()` via
`acquire_per_policy(...)` at `cli/src/main.rs:489` -- before the
`match cli.command` dispatch. The enroll arm itself
(`cli/src/main.rs:720-743`) never calls `config_read` -- it goes
straight to `load_membership_or_exit`. So the bulk subtest's
config-shaped forbidden substrings are vacuously absent for enroll
regardless of whether the lock policy is correctly declared. The
current test still catches a regression that lets membership be read
without the lock (e.g. `lock_policy(EnrollKeyFile)` mistakenly
returning `None`), but only incidentally:

1. `/var/lib/braid/pool.json` does not exist at test time (the braid
   module's tmpfiles rule at `modules/braid/storage.nix:45` only creates
   the directory), so `MembershipError::Io`'s display includes
   `"No such file or directory"`, which is in the generic forbidden
   list.
2. The positive `"another braid operation is already in progress" in out`
   assertion fails because the program would exit before reaching the
   lock.

Neither mechanism names membership reading. If a future change ever
seeds `pool.json` with valid content, mechanism 1 disappears and the
positive assertion in 2 starts passing falsely (membership load
succeeds, lock contention then surfaces, contention message appears).

This plan makes enroll's protection explicit by mirroring the
discover-write subtest pattern at lines 70-84 of the same file.

## Change

### 1. Remove the vacuous enroll case from the bulk subtest

In `tests/module/pool-lock-precedes-state-read.py`, delete the
`"enroll"` entry from the `cases` dict in the
`"fail-fast mutators acquire before broken config"` subtest (currently
line 64):

```python
"enroll": "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin",
```

The bulk subtest's contract is "broken config" -- enroll doesn't read
config, so its presence there encodes a false intent. The seven
remaining cases (unlock, add, recover, remove, remove-missing, replace,
lock) all do read config and stay in the dict; the shared
`assert_contention` helper needs no changes.

### 2. Add a dedicated enroll subtest mirroring discover-write

Insert a new subtest **immediately after** the existing
`"discover --write acquires before pending-op and probe reads"` subtest
(currently ending at line 84), before the ack subtest. Adjacency groups
the "pre-write specific state, forbid the diagnostic that would only
appear if state was read pre-lock" pattern visually.

Final shape of the new subtest:

```python
with subtest("enroll acquires before membership read"):
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("printf 'not valid json' > /var/lib/braid/pool.json")
    rc, out = with_holder(
        "printf x | braid --config /nonexistent/braid.json enroll /nonexistent/keydir --passphrase-stdin"
    )
    machine.succeed("rm -f /var/lib/braid/pool.json")
    assert rc != 0, "enroll should fail under contention; out=" + out
    assert rc != 124, "enroll hung past contention; out=" + out
    assert "pool membership file corrupt at" not in out, (
        "enroll read membership before acquiring lock; out=" + out
    )
    assert "failed to read pool membership file at" not in out, (
        "enroll read membership before acquiring lock; out=" + out
    )
    assert "failed to read config file" not in out, (
        "enroll read config before acquiring lock; out=" + out
    )
    assert "another braid operation is already in progress" in out, (
        "expected contention message; out=" + out
    )
```

Notes on each choice:

- **Pre-write `'not valid json'`** -- triggers `MembershipError::Corrupt`
  in `cli/src/membership.rs:430-445` (`serde_json::from_str` fails after
  the file read succeeds). The Corrupt display
  (`cli/src/membership.rs:32-35`) begins with `pool membership file
  corrupt at`, which is the precise signal a regression would emit.
- **Cleanup before asserts** -- matches the discover-write subtest
  (line 74). Ensures pool.json is removed even if a later assert fails,
  so subsequent subtests start clean. `rm -f` is idempotent.
- **Keep `--config /nonexistent/braid.json`** -- defensive guard the
  bulk subtest provided that this plan must preserve. Enroll's
  dispatch arm doesn't read config today (`cli/src/main.rs:720-743`),
  but a future change that started reading config before the lock
  would emit `failed to read config file /nonexistent/braid.json: ...`
  -- the new forbidden assertion catches that. Diverges intentionally
  from the discover-write subtest, which doesn't need this guard
  because discover already has `--write` as its mutation-mode flag.
- **`--passphrase-stdin` kept** -- defensive only. Clap does not
  require either `--passphrase-stdin` or `--passphrase-file`;
  `PassphraseInputArgs` at `cli/src/main.rs:163-171` only declares
  `conflicts_with` between them. Without either flag the passphrase
  path falls through to `tty.read_tty(...)` at `cli/src/luks.rs:324`,
  which would hang or fail noninteractively if execution ever reached
  passphrase handling. Under correct ordering execution never reaches
  passphrase handling under contention (lock acquire exits first), but
  keeping the flag insulates the test from any future change that
  moves passphrase resolution earlier in the arm. The `printf x` pipes
  a dummy byte that braid never reads under contention.
- **`/nonexistent/keydir`** -- enroll only opens the key file deep
  inside `cmd_enroll_key_file`, well past the membership load. Under
  contention this argument is never inspected.
- **Forbid both Corrupt and Io prefixes** -- belt-and-braces against
  future setup changes (e.g. someone switches from "pre-write invalid"
  to "rm pool.json", which would surface
  `failed to read pool membership file at` from
  `cli/src/membership.rs:51-56` instead). Both strings are guaranteed
  absent in the success path because the top-level
  `acquire_per_policy(...)` call at `cli/src/main.rs:489` exits before
  the enroll arm runs.
- **Assertion order: forbidden substrings before the contention
  message** -- under a regression where membership is read without
  the lock held (e.g. `lock_policy(EnrollKeyFile)` mistakenly
  returning `None`), the program would exit emitting a
  `MembershipError::Corrupt` diagnostic and never print the contention
  message. Asserting forbidden substrings before the positive
  contention check means a regression fires the root-cause-naming
  assertion (`"enroll read membership before acquiring lock"`) first
  rather than the generic `"expected contention message"`. Under
  correct ordering all asserts pass; under regression the most
  diagnostic one fires first. Diverges intentionally from the
  discover-write subtest's order (lines 75-84), which asserts
  contention before forbidden; the diagnostic improvement outweighs
  cross-subtest consistency in the same file.
- **Subtest label "enroll acquires before membership read"** -- matches
  the file-level preamble vocabulary ("config, membership, pending
  journals, probes, or passphrases") rather than naming the on-disk
  artifact `pool.json`.
- **No per-subtest preamble comment** -- convention in this file
  (discover-write, ack, monitor subtests have only their
  `with subtest("...")` label).

### What does not change

- `assert_contention` helper (lines 39-53) -- still serves the seven
  remaining config-reading mutators with no modification.
- `with_holder` helper (lines 23-36) -- already exactly the right
  primitive for this subtest.
- The Rust dispatch arm at `cli/src/main.rs:720-743`, its
  centralized `lock_policy(EnrollKeyFile) -> NonBlocking` declaration
  at `cli/src/main.rs:140-146`, and the top-level
  `acquire_per_policy(...)` call at `cli/src/main.rs:489` -- all
  already correct; this change is test-only.
- The sibling `tests/module/pool-lock-enroll-contention.py` test --
  out of scope; that test pins a different property (enroll fails
  fast under contention, doesn't touch the key file path) and is fine
  as-is.

## Critical files

- `tests/module/pool-lock-precedes-state-read.py` -- the only file
  edited. Two changes: remove one dict entry (line 64), insert one new
  subtest block (after line 84).

## Reference points

- Pattern being mirrored:
  `tests/module/pool-lock-precedes-state-read.py:70-84`
  (discover-write subtest).
- Helper reused: `with_holder` at
  `tests/module/pool-lock-precedes-state-read.py:23-36`.
- Error strings being forbidden derived from:
  `cli/src/membership.rs:32-35` (Corrupt display) and
  `cli/src/membership.rs:51-56` (Io display).
- Dispatch ordering being pinned: `lock_policy(EnrollKeyFile) ->
  NonBlocking` at `cli/src/main.rs:140-146`, top-level
  `acquire_per_policy(...)` at `cli/src/main.rs:489`, and the enroll
  arm starting with `load_membership_or_exit` at
  `cli/src/main.rs:720-721`.

## Verification

1. **Sanity-check the new subtest catches the regression** (manual,
   pre-merge):
   - Edit `cli/src/main.rs:140-146` locally to make `EnrollKeyFile`'s
     `lock_policy` branch always return `LockPolicy::None`. This
     simulates forgetting to declare enroll's serialization
     requirement; the top-level `acquire_per_policy(...)` then skips
     lock acquisition for enroll, and the dispatch arm reads
     `pool.json` while the holder process still owns the lock.
   - Run `just test-vm pool-lock-precedes-state-read`.
   - Expected: the new `"enroll acquires before membership read"`
     subtest fails with the assertion
     `"enroll read membership before acquiring lock; ..."`. The bulk
     mutators subtest should still pass; this edit only affects
     `EnrollKeyFile`'s policy, so the seven other commands continue to
     acquire the lock at the top of `main()` and fail with contention
     before their cmd_* would read config.
   - Revert the policy change before committing.

2. **Confirm the new subtest passes with the policy correctly
   declared**:
   - With the regression edit reverted (`lock_policy(EnrollKeyFile)`
     back to `NonBlocking` when not in dry-run), run
     `just test-vm pool-lock-precedes-state-read`.
   - All five subtests (fail-fast mutators, discover-write, the new
     enroll subtest, ack, monitor) should pass.

3. **Confirm the broader test suite is unaffected**:
   - `just test-vm` -- full VM test suite still green.
   - `just test-rust` -- no Rust changes were made, but run as a
     belt-and-braces check that nothing transitively broke.

4. **No fixture refresh needed** -- this change touches no parser-
   critical tool versions, so the parser fixture lanes
   (`just test-parsers`, `just test-rust`) are unaffected.
