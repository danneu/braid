# Plan: make the `AckError::Io` / `AckError::CleanupFailed` boundary compile-time enforced

## Context

`cli/src/ack.rs` carries two `AckError` variants that both wrap
`std::io::Error`:

- `Io(#[from] std::io::Error)` at `cli/src/ack.rs:268-270` -- the
  `#[from]` catches every `?`-propagated `io::Error` in the module
  today. The three intended sites are the pre-cleanup state-load/save
  callsites at `cli/src/ack.rs:98` (`save_acked_stats` after snapshot),
  `:159` (`load_acked_stats_fallible` in offline path), and `:163`
  (`save_acked_stats` in offline path).
- `CleanupFailed(#[source] std::io::Error)` at `cli/src/ack.rs:271-286`
  -- explicitly wrapped at the three cleanup callsites
  (`cli/src/ack.rs:57`, `:101`, `:167`) via `if let Err(e) = ... {
  return Err(AckError::CleanupFailed(e)); }`. Has a long block doc
  explaining the retry signal and the recovery contract.

The split is deliberate and pinned by tests (12+
`matches!(err, AckError::CleanupFailed(_))` assertions and one
`AckError::Io(_)` assertion at `cli/src/ack.rs:1863`). A test-internal
comment at `cli/src/ack.rs:870` even calls out "Witnesses for why
CleanupFailed is distinct from `AckError::Io`".

The problem is not just missing documentation -- it is that `#[from]`
makes `Io` the silent default for any `?` on an `io::Error`. A future
contributor adding a new cleanup-phase `?` propagation (for instance, a
helper that returns `io::Error` and gets called outside the three
existing `if let Err(e)` patterns) would silently land in `Io` and
bypass the `CleanupFailed` partial-state recovery message. Existing
tests pin the *current* cleanup callsites but cannot catch a regression
on a new code path.

The ideal fix turns the documented boundary into a compile-time guard:
drop `#[from]` so `From<std::io::Error> for AckError` no longer exists,
force the three intended pre-cleanup callsites to opt in explicitly via
`map_err(AckError::Io)?`, and any future `?` on an `io::Error` becomes a
compile error that makes the contributor choose `Io` or `CleanupFailed`
deliberately. Keep a short `///` doc comment on `Io` so the variant
self-documents next to its sibling.

## Change

`cli/src/ack.rs` only. Two edits.

### 1. Drop `#[from]` from `AckError::Io`, keep `#[source]`, and add a doc comment

At `cli/src/ack.rs:268-270`, replace:

```rust
#[error("I/O error: {0}")]
Io(#[from] std::io::Error),
```

with:

```rust
/// Pre-cleanup state-load/save I/O failure. `#[from]` is deliberately
/// omitted so `AckError`-returning code must use `map_err(AckError::Io)`
/// or wrap as `CleanupFailed`; a new `?` propagating an `io::Error` into
/// `AckError` cannot silently bypass the partial-state recovery message.
#[error("I/O error: {0}")]
Io(#[source] std::io::Error),
```

`#[source]` is kept because `thiserror`'s `#[from]` implicitly marks the
field as the error source -- dropping `#[from]` without re-adding
`#[source]` would silently empty `std::error::Error::source()` and break
the error chain. The `#[error("I/O error: {0}")]` message and the
variant's destructure shape stay the same, so the
`matches!(result, Err(AckError::Io(_)))` assertion at
`cli/src/ack.rs:1863` and the user-visible error text are unaffected.

### 2. Map the three intended pre-cleanup sites explicitly

These are the only `?`-propagated `io::Error` sites in the file
(verified: `grep -n 'std::io::Error\|io::Error' cli/src/ack.rs` plus
`grep -rn 'AckError::from\|: AckError = .*\.into\|Into<AckError>'
cli/src/` returns no other callers relying on the dropped `From` impl).
`cleanup_alert_files_and_beeper`'s `io::Error` return is *not* affected
because all three callers already wrap explicitly with
`AckError::CleanupFailed(e)`.

- `cli/src/ack.rs:98`
  `save_acked_stats(&new_acked, paths)?;`
  becomes
  `save_acked_stats(&new_acked, paths).map_err(AckError::Io)?;`
- `cli/src/ack.rs:159`
  `let mut acked = load_acked_stats_fallible(paths)?;`
  becomes
  `let mut acked = load_acked_stats_fallible(paths).map_err(AckError::Io)?;`
- `cli/src/ack.rs:163`
  `save_acked_stats(&acked, paths)?;`
  becomes
  `save_acked_stats(&acked, paths).map_err(AckError::Io)?;`

No new tests. Existing assertions cover the behavior:
`ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause`
at `cli/src/ack.rs:1848` pins `AckError::Io(_)` for the offline
load-fallible path, and the 12+ `CleanupFailed` matchers pin the
cleanup phase.

## Why not the documentation-only fix

A `///` doc comment with `#[from]` left in place is the lighter diff,
but it leaves `Io` as the silent default: a future cleanup-phase `?`
still compiles and still bypasses the `CleanupFailed` recovery message.
The compiler is a stronger reviewer than a doc comment, the diff is
still three `map_err` calls in the same module as the doc comment, and
the boundary becomes self-enforcing.

## Why not the other refactors

- **Renaming `Io`** (e.g. `StateIo`) does not solve the phase-boundary
  problem; new `?`s would still land in the renamed variant if
  `#[from]` stayed. Pure cosmetic churn.
- **Pushing wrapping into `cleanup_alert_files_and_beeper`** (returning
  `AckError` directly) couples the helper to the caller's error type
  for no behavioral gain.

## Critical file

- `cli/src/ack.rs` -- one edit on the `AckError::Io` variant, three
  `map_err(AckError::Io)` insertions at lines 98, 159, 163.

## Verification

- `cargo build -p braid-cli` (via `just test-rust`) -- removing
  `#[from]` removes the `From<std::io::Error> for AckError` impl, so
  the build passing proves no `AckError`-returning path can implicitly
  convert an `io::Error`. The `?` sites inside
  `cleanup_alert_files_and_beeper` at `cli/src/ack.rs:211-217` remain
  intentional same-type propagation -- that helper returns
  `Result<(), std::io::Error>`, and its three callers (`:57`, `:101`,
  `:167`) still explicitly wrap the result as
  `AckError::CleanupFailed(e)`.
- `just test-rust` -- existing assertions are sufficient:
  `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause`
  pins `AckError::Io(_)` for one of the three mapped sites, and the
  12+ `matches!(err, AckError::CleanupFailed(_))` tests pin that the
  cleanup phase still routes correctly.
- No VM test run is required: this change touches no behavior, no
  external tool, and no module crossing the parser-compatibility lane.
