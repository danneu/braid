# Plan: collapse duplicate deadline-zero guards in `run_systemd_stop_lock`

## Context

A code-review finding flagged that `run_systemd_stop_lock` in
`cli/src/main.rs` contains two structurally adjacent guards that print
the same `PoolLockError::DeadlineExpired { waited: deadline }` line and
exit 1:

```rust
let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
    eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
    std::process::exit(1);
};
if remaining.is_zero() {
    eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
    std::process::exit(1);
}
```

The `None` branch covers `elapsed > deadline` (underflow). The
`is_zero()` branch covers `elapsed == deadline`. Both report the same
condition ("no time budget left") but use different shapes -- a
maintainer reading this for the first time has to step through both
`<= 0` cases to convince themselves the second guard is just an
early-exit optimization, not a correctness fix the `let else` missed.

The finding proposed deleting the `is_zero` guard outright. That's
wrong in one corner: `RealPoolLock::poll_acquire` returns `Ok(guard)`
unconditionally on a successful `try_acquire` before consulting the
timeout, so with `remaining == Duration::ZERO` and an uncontended pool
lock the wrapper would proceed into `cmd_lock` with zero budget left
rather than aborting. The pivot below collapses the two guards into one
without changing observable behavior.

## The pivot

Replace `checked_sub` + separate `is_zero` guard with `saturating_sub`
+ single `is_zero` guard. The three relevant cases:

| `start.elapsed()` vs `deadline` | `checked_sub` today                 | `saturating_sub` after  | Outcome (both)           |
| ------------------------------- | ----------------------------------- | ----------------------- | ------------------------ |
| elapsed > deadline              | `None` -> exit                      | `ZERO` -> `is_zero` exit | abort with same stderr   |
| elapsed == deadline             | `Some(ZERO)` -> `is_zero` exit      | `ZERO` -> `is_zero` exit | abort with same stderr   |
| elapsed < deadline              | `Some(>0)` -> proceed               | `>0` -> proceed         | acquire with same budget |

Behavior is byte-identical in every case. The reader sees one branch
covering "no time budget left," not two adjacent branches they have to
reconcile.

## Critical files

- `cli/src/main.rs` -- `run_systemd_stop_lock` at lines 1174-1226.
  Replace lines 1199-1206 (the `let else` + `if is_zero` pair) with:

  ```rust
  let remaining = deadline.saturating_sub(start.elapsed());
  if remaining.is_zero() {
      eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
      std::process::exit(1);
  }
  ```

No other files change. The four `DeadlineExpired { waited: deadline }`
emit sites in this function (1189-1191, 1199-1202, 1203-1206,
1209-1212) collapse to three after this pivot; the project's
established convention is plain helper functions rather than
`|| -> !` closures (zero occurrences of the latter in the crate), so
the remaining three sites are left as-is rather than abstracted -- the
duplication that mattered for readability was the two adjacent
zero-time guards, not the four overall emit sites.

## Out of scope

- The finding mis-cites the line range as `1041-1048` (which is
  `acquire_pool_or_exit`). The actual code is at `1199-1206`. We are
  not editing the finding -- only the code.
- No new helper, no extracted closure for the `DeadlineExpired`
  emit sites. The codebase uses plain functions like
  `print_cli_error` / `handle_pool_lock_error` for shared exit
  glue; a one-shot closure here would be an outlier.
- No change to `PoolLockError::DeadlineExpired`'s `Display` impl or
  to `acquire_with_systemd_stop_deadline` / `poll_acquire`. The
  refactor is local to one function.

## Verification

1. `just test-rust` -- `cli/src/pool_lock.rs:308-357` covers
   `acquire_with_systemd_stop_deadline` returning `DeadlineExpired`
   on expiry and acquiring after holder release. These unit tests are
   the contract this refactor must not break.
2. `just test-vm braid-lock-systemd-stop` -- the VM test at
   `tests/module/braid-lock-systemd-stop.py:45-59` drives a real
   ExecStop against a held `flock` and asserts the journal contains
   the substring `"aborting --systemd-stop"` (the trailing part of
   `PoolLockError::DeadlineExpired`'s `Display` impl at
   `cli/src/pool_lock.rs:29`). This is the end-to-end pin for the
   stderr message; the refactor preserves it exactly.
3. `cargo check -p braid-cli` -- compile sanity. `Duration::saturating_sub`
   is stable since Rust 1.53; no toolchain concern.

No new tests are added: the existing unit + VM coverage already
exercises both the `elapsed > deadline` and `elapsed < deadline`
paths through the wrapper, and the `elapsed == deadline` boundary is
unreachable to assert against without a timing-fragile harness. The
refactor is a structural collapse, not a behavior change.
