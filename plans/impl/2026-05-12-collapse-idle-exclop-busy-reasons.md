# Plan: collapse idle exclop busy reasons

## Summary

Refactor `BusyReason` so btrfs exclusive-operation busy states are
represented as `BusyReason::Exclop(ExclusiveOp)` instead of mirrored
variants. Preserve the current `braid idle` stdout exactly, including
`busy: balance running`, `busy: balance paused`, and
`busy: device add in progress`.

## Key changes

- Promote `preflight::ExclusiveOp` from `pub(crate)` to `pub` because
  `BusyReason` is public through `braid_cli::idle`, and a public enum
  variant cannot cleanly expose a crate-private payload.
- Replace the seven exclop variants in `BusyReason` with:

  ```rust
  Exclop(ExclusiveOp)
  ```

- Remove `busy_from_exclop`; in `cmd_idle`, map busy exclops directly:

  ```rust
  Err(ExclusiveOpError::Busy(op)) => return IdleResult::Busy(BusyReason::Exclop(op)),
  ```

- Keep `ExclusiveOp::Display` as the canonical operation-name renderer
  used by preflight messages.
- Implement `BusyReason::Display` with idle-specific text:
  - `Balance` -> `balance running`
  - `BalancePaused` -> `balance paused`
  - all other exclops -> `{op} in progress`
- Preserve the already-staged sysfs-before-scrub probe-order changes;
  only adjust their expected `BusyReason` constructors.
- Update `manual/commands/idle.md` under "What happens under the hood"
  so the documented order is mountinfo check -> sysfs exclusive-op scan
  -> scrub status, and state that a sysfs busy result short-circuits
  before the scrub probe.

## Tests

- Update existing idle unit tests to expect
  `BusyReason::Exclop(ExclusiveOp::...)`.
- Add a focused `BusyReason` display test that pins the documented CLI
  strings for scrub, balance, paused balance, each generic exclop, and
  unknown.
- Keep the staged request-count assertions intact so the probe-order
  behavior remains covered.
- Run `just test-rust`.

## Assumptions

- No decision-doc update is needed because the sysfs-before-scrub
  behavior is already represented by the staged code/tests and the
  required user-facing doc correction belongs in the command manual.
- The public visibility change for `ExclusiveOp` is acceptable because
  the crate already exposes `IdleResult` and `BusyReason` through the
  public library boundary, and braid has no backwards-compatibility
  requirement.
