# Plan: drop the dead `_membership` parameter from `LockPlan::execute`

## Context

Commit `52f3932 fix(lock): derive already-closed rows in planner` (Mon May 18)
moved every membership-driven decision in `braid lock` into `plan_lock`. After
that migration, `LockPlan::execute` no longer needed `&PoolMembership`: the
close set, the `members_known_closed` rows, and the skipped-mapper list are
all planner-derived on the `LockPlan` itself, and the invariant comment at
`cli/src/lock.rs:601-603` explicitly states "execute never infers absence from
reconstructed mapper names after scan or classification uncertainty."

The migration commit left `_membership: &PoolMembership` on the `execute`
signature with an underscore prefix -- vestigial, not load-bearing. It costs
nothing today, but it lies about the data flow: a reader can reasonably
suspect that execute might still consult membership at run time (which would
be a bug per the stated invariant) and the inert argument keeps showing up
in code-review findings.

The fix is mechanical and minimal: remove the parameter from
`LockPlan::execute` and from its two call sites. `cmd_lock`, `cmd_lock_impl`,
and the `run_plain_lock` / `run_systemd_stop_lock` orchestrators in `main.rs`
keep their `membership` parameter -- they still feed it into `plan_lock`, which
is its only live consumer.

## Scope

Two-line behavioral footprint: the parameter at the `execute` signature and
the matching argument at each call site. Nothing else moves.

### In scope

- `LockPlan::execute` signature change (one line removed).
- Two call sites of `plan.execute` updated to drop the now-removed argument.

### Out of scope

- `cmd_lock` (`cli/src/lock.rs:960`) -- keeps `membership` (passes to
  `cmd_lock_impl`).
- `cmd_lock_impl` (`cli/src/lock.rs:970`) -- keeps `membership` (passes to
  `plan_lock` at line 988, which is its only live use).
- `run_plain_lock` (`cli/src/main.rs:981`) and `run_systemd_stop_lock`
  (`cli/src/main.rs:1016`) -- keep `membership` (forward to `cmd_lock`).
- The `LockPlan` struct, `plan_lock`, and the close-set builders -- none of
  them touch the parameter.
- The original `verify-issue` finding's proposed scope ("update the four call
  sites: `cmd_lock`, `cmd_lock_impl`, plus the two `run_*_lock`
  orchestrators") is incorrect. Following it verbatim would either fail to
  compile (if `membership` were actually stripped from `cmd_lock_impl`) or
  be a non-change at those sites. We do not adopt that scope.

## Changes

### 1. `cli/src/lock.rs:492-503` -- `LockPlan::execute` signature

Remove the `_membership: &PoolMembership` parameter line. The function body
already does not reference it; no body changes are required.

Before:

```rust
pub(crate) fn execute<R, F, S>(
    self,
    runner: &R,
    fs: &F,
    sleeper: &S,
    _membership: &PoolMembership,
) -> Result<(), LockError>
```

After:

```rust
pub(crate) fn execute<R, F, S>(
    self,
    runner: &R,
    fs: &F,
    sleeper: &S,
) -> Result<(), LockError>
```

### 2. `cli/src/lock.rs:993` -- production call site in `cmd_lock_impl`

Before:

```rust
plan.execute(runner, fs, sleeper, membership)
```

After:

```rust
plan.execute(runner, fs, sleeper)
```

Note: `cmd_lock_impl`'s `membership` parameter remains because line 988
(`let plan = plan_lock(runner, fs, config, membership)?;`) still requires it.

### 3. `cli/src/lock.rs:1539` -- test call site in `execute_does_not_close_membership_mapper_absent_from_plan`

Before:

```rust
plan.execute(&recording, &execute_fs, &LockNoopSleeper, &membership)
    .expect("execute should succeed without closing the unplanned mapper");
```

After:

```rust
plan.execute(&recording, &execute_fs, &LockNoopSleeper)
    .expect("execute should succeed without closing the unplanned mapper");
```

The surrounding test fixture (`let membership = lock_test_membership();` at
line 1528 and the `plan_lock(&runner, &plan_fs, &config, &membership)` at
line 1531) stays -- `plan_lock` still consumes membership, so the local
binding remains useful for the planner setup even though it is no longer
threaded into `execute`. No test assertion changes.

## Critical files

- `cli/src/lock.rs` -- only file that needs editing. Three edits total.

No edits to:
- `cli/src/main.rs` (orchestrators keep their `membership` plumbing as-is)
- `cli/src/membership.rs`, `cli/src/probe.rs`, or any other module
- `docs/` -- no design doc references `LockPlan::execute(..., membership)`
  by signature; the existing invariant comment at `cli/src/lock.rs:601-603`
  remains accurate and needs no change

## Verification

Behavioral tests that exercise `LockPlan::execute` through `cmd_lock_impl`:

1. **Rust unit tests:** `just test-rust` -- covers
   `execute_does_not_close_membership_mapper_absent_from_plan` (the one
   direct caller of `LockPlan::execute` in tests) plus the ~20 other
   `cmd_lock_impl`-based unit tests in `cli/src/lock.rs` that go through
   the production call site at line 993. A green run proves the
   compile-time signature change did not regress execute behavior.

2. **VM test for the already-closed planner invariant:**
   `just test-vm luks-lock-skipped-no-false-closed` -- added in the same
   commit (`52f3932`) that introduced `_membership`. Verifies that
   planner-derived already-closed rows print correctly without execute-time
   membership reinterpretation. This is the closest behavioral test to the
   parameter we are removing.

3. **Full lock VM coverage:** `just test-vm` (no filter) -- sweeps every
   `luks-lock-*` test in `tests/cli/`. Runtime is acceptable; do this once
   before commit to confirm no other lock path is implicated.

Expected outcome: all three green with no behavioral diff. No fixture
refresh needed (parser-critical tool versions are untouched). No
documentation changes required.
