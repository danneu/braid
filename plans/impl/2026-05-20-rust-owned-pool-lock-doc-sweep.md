# Plan: sweep stale "wrapper" attributions from test prose and two Rust sites

## Context

Commit `ff6f766 fix(lock): move pool lock ownership into rust dispatch` moved
`/run/braid-pool.lock` acquisition, post-success `braid-online.service`
activation, and `pool_access_group` permission fixup out of
`modules/braid/braid-wrapper.sh` and into Rust dispatch
(`cli/src/main.rs::main` -> `acquire_pool_or_exit` -> `RealPoolLock` in
`cli/src/pool_lock.rs`; post-lock work via `mark_online`/`mark_offline` in
`cli/src/online_state.rs`). The wrapper is now a three-line PATH+exec shim.
ADR 026 (`docs/decisions/026-pool-lock-rust-owned.md`) is the formal record;
ADR 018 was already updated to reflect the new ownership.

Test preambles and inline comments across `tests/module/` still describe the
wrapper as the locking authority, the scrub-stop authority, and the mount-
permission-fixup authority. AGENTS.md "Test Conventions" makes the
Intent/Why-it-exists/Scenario preamble load-bearing for future maintainers
deciding whether a failing test still protects against the regression it was
written for. The wrapper-as-authority prose makes that question hard to
answer, even though the test logic still exercises the correct path (the
contention string is pinned verbatim in
`cli/src/pool_lock.rs::PoolLockError::AlreadyHeld`).

Two Rust sites share the same root cause and get swept together:
- `cli/src/unlock.rs:135` -- a comment that says "the wrapper holds the pool
  flock for the lifetime of unlock".
- `cli/src/pool_lock.rs:304` -- the unit test name
  `already_held_display_is_wrapper_compatible_verbatim`, which now misleads.

No code logic changes. Test behavior stays untouched; only comments, the
Group B informational `assert`/`print` labels, and the Group C Rust test
function rename change. Outcome: a future
maintainer reading any of these tests can immediately answer "what does this
test protect against, and where does the code live?" without grepping for a
wrapper case statement that no longer exists.

## Scope and style

- **Terse rewrites in the style of `tests/module/braid-pool-lock-not-inherited.py`
  and `tests/module/pool-lock-precedes-state-read.py`**: phrases like "Rust
  dispatch owns the pool lock" or "Rust dispatch acquires
  `/run/braid-pool.lock` before ...". No `file:line` citations inline.
- **ADR 026 pointer (`docs/decisions/026-pool-lock-rust-owned.md`) only on
  the longer preambles** where the "why" is non-obvious -- specifically
  `systemd-lifecycle.{py,nix}`, `alert-state-lock.{py,nix}`,
  `pool-lock-contention.{py,nix}`, and `pool-lock-replace-contention.{py,nix}`.
  Short preambles do not need it.
- **Test behavior, fixtures, expected command output, control flow, and
  `nodes.machine` configs stay byte-identical.** What may change: `#`
  preamble and inline comments; the human-readable Python f-string passed
  to Python `assert` messages or `print` labels (purely informational and
  surfaced only on failure / for debugging), strictly limited to the sites
  itemized in Group B; and the single Rust test function rename itemized
  in Group C. The contention strings emitted by `braid` itself (e.g.
  `"another braid operation is already in progress"`) are pinned in
  `cli/src/pool_lock.rs::PoolLockError` and stay byte-identical -- this
  plan does not touch them.
- **Preserve historic-context phrasing where it is intentional.** Examples
  that stay as-is (already verified out of scope):
  `tests/module/braid-alert.{py,nix}` (braid-beep-probe wrapper),
  `tests/module/smartd-config.{py,nix}` (sendmail wrapper),
  `tests/module/fan-control.py` (NixOS `script =` wrapper),
  `tests/module/ups-credential-lifecycle.py:8` (UPS module ref),
  `tests/module/ups-preflight-on-battery.py:13` (still-accurate PATH shim),
  `tests/cli/braid-unlock.py:164` (intentionally historic: "was removed").

## Files to edit

### Group A -- preambles (file-level intent comments)

Rewrite the leading Intent/Why-it-exists/Scenario block (or `# What/Why` for
`.nix`) so the authority lines name Rust dispatch instead of the wrapper.
Keep the existing scenario detail; only attribution shifts.

- `tests/module/systemd-lifecycle.py:1-29` -- holistic preamble rewrite. Drop
  "wrapper script" from the three-moving-parts list (now two: target,
  services). Subtest enumeration items (3), (4), (5), (6) currently
  attribute activation/contention to the wrapper; reword to "Rust dispatch".
  Add ADR 026 pointer.
- `tests/module/systemd-lifecycle.nix:1-9` -- preamble mirrors the `.py`;
  shift "CLI wrapper synchronization" to "Rust dispatch synchronization".
- `tests/module/pool-lock-discover-contention.py:1-16` -- already mostly
  correct ("Rust-owned pool lock"); only line 69's inline assertion message
  is stale. See Group B.
- `tests/module/pool-lock-replace-contention.py:1-16` -- "fail at the
  wrapper lock" -> "fail at the Rust-owned pool lock". Add ADR 026 pointer.
- `tests/module/pool-lock-replace-contention.nix:1-9` -- "takes the wrapper
  pool lock" -> "takes the Rust-owned pool lock"; "wrapper-level
  serialization" -> "Rust-level serialization".
- `tests/module/pool-lock-contention.py:1-14` -- "the wrapper must fail
  fast" -> "Rust dispatch must fail fast"; final paragraph similar. Add
  ADR 026 pointer.
- `tests/module/pool-lock-contention.nix:1-9` -- "wrapper's flock
  acquisition" -> "Rust dispatch's pool-lock acquisition"; "wrapper
  regresses" -> "dispatch regresses".
- `tests/module/alert-state-lock.py:1-15` -- "wrapper-level serialization"
  -> "Rust-level serialization, owned by the pool lock". Add ADR 026
  pointer.
- `tests/module/alert-state-lock.nix:1-9` -- "the wrapper's
  /run/braid-pool.lock" -> "the Rust-owned `/run/braid-pool.lock`"; "wrapper
  lock is the serialization boundary" -> "pool lock is the serialization
  boundary".
- `tests/module/scrub-lifecycle.py:1-26` -- "wrapper stops the timer and
  service first" -> "Rust dispatch stops the timer and service first";
  cancel-node bullet on line 21 same shift.
- `tests/module/scrub-lifecycle.nix:1-21` -- same shift on the cancel-node
  bullet (line 16).
- `tests/module/add-bootstrap.py:1-11` -- "the wrapper sets mount point
  permissions" -> "Rust dispatch sets mount point permissions" (concretely:
  `mark_online` via `pool_access_group`). "wrapper-based permission fixup"
  -> "Rust-side permission fixup".
- `tests/module/add-bootstrap.nix:1-13` -- same shift.

### Group B -- inline comments inside subtests

- `tests/module/systemd-lifecycle.py:91` -- "The wrapper's post-lock
  `systemctl stop braid-online` is a no-op" -> "The post-lock
  `systemctl stop braid-online` from `mark_offline` is a no-op".
- `tests/module/systemd-lifecycle.py:119, 127` -- subtest headers "CLI
  wrapper synchronization (unlock/lock)" -> "CLI dispatch synchronization".
- `tests/module/systemd-lifecycle.py:141` -- "The wrapper's non-blocking
  flock on /run/braid-pool.lock" -> "The Rust-owned non-blocking flock on
  /run/braid-pool.lock".
- `tests/module/systemd-lifecycle.py:217, 230` -- "bypassing the wrapper so
  braid-online stays inactive" / "wrapper didn't run" -> "bypassing Rust
  dispatch so `mark_online` does not run".
- `tests/module/systemd-lifecycle.py:234` -- "Add a 3rd disk through the
  wrapper -- this must activate braid-online" -> "Add a 3rd disk through
  Rust dispatch -- `mark_online` must activate braid-online".
- `tests/module/systemd-lifecycle.py:247, 252` -- subtest header "Negative
  path -- wrapper activation failure" / "the wrapper's WARNING code path"
  -> "Negative path -- braid-online activation failure" / "Rust
  dispatch's WARNING code path".
- `tests/module/systemd-lifecycle.py:277` -- `print(f"Wrapper output:...")`
  is a debug print on captured stdout; rename to `f"Unlock output:..."`.
- `tests/module/systemd-lifecycle.py:304, 324` -- "clear recovery mode
  through the wrapper" / "Recover through the wrapper" -> "clear recovery
  mode through Rust dispatch" / "Recover through Rust dispatch".
- `tests/module/systemd-lifecycle.py:340` -- "the wrapper's non-blocking
  flock must let exactly one win" -> "the Rust-owned non-blocking flock
  must let exactly one win".
- `tests/module/systemd-lifecycle.py:422` -- "The wrapper's flock check
  fires BEFORE the CLI writes its journal" -> "Rust dispatch acquires the
  pool lock BEFORE it writes the journal".
- `tests/module/pool-lock-contention.py:60, 72` -- "if the wrapper
  regresses to blocking flock" / "wrapper regressed to blocking flock" ->
  "if Rust dispatch regresses to blocking flock" / "Rust dispatch
  regressed to blocking flock".
- `tests/module/pool-lock-discover-contention.py:69` -- assertion message
  "wrapper lock check must not fire after release" -> "pool lock check
  must not fire after release".
- `tests/module/scrub-lifecycle.py:257` -- "The wrapper must stop the
  timer and service before CLI attempts unmount" -> "Rust dispatch must
  stop the timer and service before it attempts unmount".

### Group C -- Rust sites (carry the same staleness)

- `cli/src/unlock.rs:135` -- inline comment "the wrapper holds the pool
  flock for the lifetime of unlock" -> "Rust dispatch holds the pool
  flock for the lifetime of unlock".
- `cli/src/pool_lock.rs:303-309` -- rename test fn from
  `already_held_display_is_wrapper_compatible_verbatim` to
  `already_held_display_matches_pinned_contention_string`. Assertion body
  unchanged.

## Verification

This is comments-only and a single test rename. The verification surface is:

1. **Rust unit tests compile and the renamed test passes**:
   - `just test-rust` -- confirms the rename in `cli/src/pool_lock.rs` and
     the comment edit in `cli/src/unlock.rs` did not break compilation or
     other tests that reference the old name. Expected: clean pass.
   - Confirm no source / test call sites still reference the old test name
     (scope the search to source paths so historical plan / doc references
     to the old name in `plans/` and `docs/` do not produce false hits):
     `rg -n 'already_held_display_is_wrapper_compatible_verbatim' cli/src cli/tests tests modules`
     should return no hits after the rename.

2. **VM tests still pass against the edited preambles**:
   - `just test-vm pool-lock-contention pool-lock-discover-contention
     pool-lock-replace-contention alert-state-lock braid-module-add-bootstrap
     scrub-lifecycle systemd-lifecycle` -- runs every edited test. Expected:
     all pass; no test logic changed.

3. **Sanity grep after the sweep** -- the only remaining "wrapper"
   mentions in `tests/module/` should be the out-of-scope ones listed in
   "Scope and style" (beep-probe, sendmail, NixOS `script =`, UPS module,
   PATH shim). Confirm with:

   ```
   rg -n '\bwrapper' tests/module/ cli/src/unlock.rs cli/src/pool_lock.rs
   ```

   Manually review remaining hits against the explicit allow-list above.

4. **AGENTS.md preamble contract**: each edited preamble still carries
   Intent, Why it exists, and Scenario (for `.py`) or `What`/`Why` (for
   `.nix`). Spot-check the three longest rewrites (`systemd-lifecycle.py`,
   `alert-state-lock.py`, `pool-lock-contention.py`).

No fixture refresh, no parser canary, no flake change. `just test-rust` plus
the targeted `just test-vm` run above is sufficient.

## Implementation notes

- Also renamed the capitalized `systemd-lifecycle.py` negative-path subtest
  label from `Wrapper warns...` to `Rust dispatch warns...`; it is the same
  stale diagnostic attribution as the planned negative-path header/comment
  rewrite and does not alter test control flow.
