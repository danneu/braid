# Plan: flatten `latch_state` to `causes` in `cmd_ack_impl`

## Context

A `/verify-issue` finding flagged that the entry block of `cmd_ack_impl`
in `cli/src/ack.rs` carries an `Option<AlertState>` (named `latch_state`)
only to extract `.causes.as_slice()` on the next line. Nothing else on
the wrapper is ever consumed inside ack -- `AlertState` is a one-field
struct holding `causes: Vec<AlertCause>` (`cli/src/alert.rs:14-17`).

The current shape:

- spreads the latch read across a 12-line tuple/match plus a separate
  `Option::as_ref().map(...).unwrap_or(&[])` chain (`cli/src/ack.rs:35-46`),
- forces every reader to trace the `as_ref().map().unwrap_or(&[])`
  follow-up to confirm `latch_state` itself is never read again, and
- would mislead future maintainers if a new field were added to
  `AlertState` (e.g. timestamps) into thinking ack consumes the
  latch-as-data when it only consumes the causes vector.

Sibling check confirmed the wrapper is genuinely used elsewhere
(`cli/src/monitor.rs:144` passes `existing_latch.as_ref()` to
`merge_into_latch`; `cli/src/status.rs:579-618` returns an `AlertState`)
-- this is not a wider pattern to unify, only one local simplification
in `cmd_ack_impl`.

Outcome: a flatter entry block where the only thing carried out of the
latch read is the `Vec<AlertCause>` (and the `latch_corrupt` flag), with
no change to ack's externally observable behavior.

## Change

Single edit in `cli/src/ack.rs` (no other files).

### Replace `cli/src/ack.rs:35-46`

Current:

```rust
let (latch_state, latch_corrupt) = match alert::load_alert_latch(paths) {
    Ok(Some(s)) => (Some(s), false),
    Ok(None) => (None, false),
    Err(e) => {
        eprintln!("warning: alert latch unreadable -- treating as active for ack gating: {e}");
        (None, true)
    }
};
let causes: &[AlertCause] = latch_state
    .as_ref()
    .map(|s| s.causes.as_slice())
    .unwrap_or(&[]);
```

Proposed:

```rust
let (causes, latch_corrupt) = match alert::load_alert_latch(paths) {
    Ok(Some(s)) => (s.causes, false),
    Ok(None) => (Vec::new(), false),
    Err(e) => {
        eprintln!("warning: alert latch unreadable -- treating as active for ack gating: {e}");
        (Vec::new(), true)
    }
};
```

Type of `causes` changes from `&[AlertCause]` to `Vec<AlertCause>`. The
match takes ownership of `s.causes` rather than holding `s` to borrow
from it -- no extra allocation, the `Vec` is moved out of the dropped
`AlertState`.

### One call-site adaptation: `cli/src/ack.rs:68`

`ack_offline` keeps its `causes: &[AlertCause]` parameter
(`cli/src/ack.rs:117-124`). Update the call from `causes,` to `&causes,`
inside `cmd_ack_impl`:

```rust
return ack_offline(
    &causes,
    latch_corrupt,
    smartd_active,
    remove_smartd,
    paths,
    stop_beeper,
);
```

All other usages (`causes.iter().any(...)` line 49,
`causes.is_empty()` lines 52/77/104, `causes.len()` lines 107/108) work
unchanged on `Vec<AlertCause>` via the same method set.

### Preamble comment

The block-level comment at `cli/src/ack.rs:26-34` ("Snapshot the gating
inputs...") still applies verbatim -- the snapshot semantics, ordering
relative to `probe_pool_alerts`, and the "unreadable latch counts as
active" rule are unchanged. Leave the comment as-is.

## What this plan does NOT change

- `AlertState`'s definition or any of its other consumers
  (`cli/src/monitor.rs`, `cli/src/status.rs`, TUI modules).
- `ack_offline`'s signature or any other function signature.
- Any test fixture, helper, or assertion -- the externally observable
  behavior of `cmd_ack_impl` (gating decisions, cleanup ordering, error
  variants, printed output) is identical.
- The `eprintln!` warning text or trigger condition.

## Verification

The existing test suite already pins every behavior the change touches.
Re-run it to confirm no regression:

1. `just test-rust` -- runs the `cmd_ack_impl` unit tests including:
   - `cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`
     (`cli/src/ack.rs:349`) exercises the `Err` arm of the match -- pins
     the corrupt-latch warning + full-ack-path behavior.
   - `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`
     (`cli/src/ack.rs:433`) exercises `Ok(None)` -- empty causes vec,
     `latch_corrupt = false`, smartd flag drives the full path.
   - `cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path`
     (`cli/src/ack.rs:673`) exercises `Ok(Some(s))` with a real causes
     vec.
   - `cmd_ack_noop_when_no_alerts_does_not_query_btrfs_or_write_acked_stats`
     (`cli/src/ack.rs:321`) exercises the early-return gate where
     `causes.is_empty()` matters.
   - The offline-branch tests (`ack_offline_*`) pin the `&causes`
     hand-off into `ack_offline` -- `ack_offline_with_missing_device_preserves_mid_probe_smartd_flag`,
     `ack_offline_does_not_consume_smartd_flag_arriving_during_probe`,
     `ack_offline_retry_after_cleanup_failed_completes_recovery`, etc.
   - Retry/sentinel/cleanup-failure tests
     (`cmd_ack_mounted_retry_after_cleanup_failed_completes_recovery`,
     `cmd_ack_mounted_sentinel_only_retry_does_not_query_btrfs_or_rewrite_baseline`,
     etc.) pin the gating-input snapshot semantics this block produces.

2. `cargo check -p braid-cli` (covered by `just test-rust`'s compile
   step) catches the one signature adaptation: forgetting `&causes` on
   line 68 fails to compile because `Vec<AlertCause>` is not
   `&[AlertCause]` at the call boundary. (Deref coercion only kicks in
   for `&Vec<T>` to `&[T]`, not for an owned `Vec<T>`.)

No VM tests, no fixture refresh, no docs to touch -- the change is
entirely internal to one function in `cli/src/ack.rs`.
