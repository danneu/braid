# Plan: doc comment for `stop_beeper` (ack.rs)

## Context

A `/ultrareview` finding (Low / Simplicity) flagged
`format_systemctl_stop_failure`'s `Option<String>` -> `eprintln!` seam as
"low-value indirection" and proposed collapsing it into a `&mut dyn Write`
helper. Investigation (`/verify-issue`) found that:

- The "duplicated decision" premise is inaccurate -- the helper decides
  (`Some`/`None`); the caller only forwards and additionally owns the spawn-`Err`
  case the helper can't see.
- The proposed `&mut dyn Write` rewrite is a wash-to-regression: it trades a
  *pure* function (tested with `assert_eq!(format_systemctl_stop_failure(&o), None)`)
  for an I/O-coupled one without reducing function count.
- The genuinely non-obvious thing -- which the finding never mentioned -- is that
  `stop_beeper` shells out to host `systemctl` **directly** instead of using the
  `OnlineStateOps::systemctl_stop` abstraction that `lock.rs#stop_unit_warn_on_error`
  uses. That divergence is *deliberate and correct*, not cruft.

The code is correct; it is reviewer-confusing. The braid-appropriate remedy is a
`docs` change: one `///` comment capturing the deliberate boundary so this class
of finding cannot recur.

## Why `stop_beeper` only (not the cited formatter)

braid's doc-comment rule: "If removing the comment would not lose any information
a reader could not recover from the code, do not write it."

- **`stop_beeper`**: the direct-shell-out-vs-`OnlineStateOps` choice is *not*
  recoverable from the function in isolation. Comment it.
- **`format_systemctl_stop_failure`**: a pure formatter with `return None` on
  success as its first line and three tests directly beneath it; its separation
  is self-evident once `stop_beeper` is documented as the impure injected leaf.
  Recoverable -> skip (same reason the file already leaves `ack_offline` bare).

A single comment on `stop_beeper` dissolves the finding transitively.

## Change

File: `cli/src/ack.rs`, immediately above `fn stop_beeper()` (currently
undocumented).

```rust
/// Shells out directly rather than via `OnlineStateOps::systemctl_stop`
/// because the beeper stop also runs on the offline cleanup path
/// (`ack_offline`), which issues no `CommandRunner` requests.
fn stop_beeper() {
```

Every claim is code-backed and every reference is a drift-proof symbol
(per the File References rule -- no line numbers):

- **`OnlineStateOps::systemctl_stop`** (`cli/src/online_state.rs`) runs through a
  `CommandRunner` (`self.runner.run(&CmdRequest::SystemctlStop { .. })`).
- **offline path**: `cmd_ack_impl` -> `ack_offline` -> `cleanup_alert_files_and_beeper`
  -> `stop_beeper()`. `probe_pool_alerts` short-circuits on the not-mounted
  `Filesystem` before any runner call, so the offline branch issues zero
  `CommandRunner` requests -- enforced by `AckPanicRunner` (panics on any runner
  call) backing the offline tests. Routing the stop through `OnlineStateOps`
  would fire a runner request on that path and break the contract.
- House style: braid's good-example doc comments name the contrasting type
  ("Separate from `MountState` because ..."), so naming `OnlineStateOps::systemctl_stop`
  is on-pattern, not coupling-by-accident.

## Out of scope

- `format_systemctl_stop_failure` -- left bare (recoverable; see above). Its
  three `format_systemctl_stop_failure_*` tests already lock the behavior the
  `stop_beeper` comment references.
- `ack_offline` -- also undocumented, also recoverable; not part of this finding.
- No code/behavior change, no test change, no `Option<String>` -> `&mut dyn Write`
  rewrite.

## Verification

Non-behavioral (comment only). Confirm:

1. `just test-rust` -- crate still compiles and the existing
   `format_systemctl_stop_failure_*` tests still pass.
2. `git diff cli/src/ack.rs` -- the diff is exactly the three comment lines;
   no accidental formatter drift (do not run `cargo fmt`).
3. Read-through: comment is 3 lines, references resolve (`rg OnlineStateOps::systemctl_stop`,
   `rg 'fn ack_offline'`), no signature restatement.
