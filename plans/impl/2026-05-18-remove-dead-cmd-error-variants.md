# Plan: remove dead `Cmd` variants from RemoveMissingError and RemoveError

## Context

A simplicity finding flagged `RemoveMissingError::Cmd` (`cli/src/remove_missing.rs:46-47`) as a dead error variant. Verification confirmed it is genuinely unreachable: the only `runner.run` call in the module (`check_relocation_space` at line 563) explicitly matches on the result and converts `CmdError` into `RelocationCheck::ProceedWithWarning(...)` rather than propagating it via `?`. No other call site in the module returns `Result<_, CmdError>`, no code in the crate constructs `RemoveMissingError::Cmd` explicitly, and no tests match on it.

The sibling module `cli/src/remove.rs` has the identical dead variant (`RemoveError::Cmd` at line 45-46) for the same reason: its only `runner.run` (the eviction-check helper at line 697) routes `CmdError` through `EvictionCheck::ProceedWithWarning`. Both variants were introduced when these modules used direct `?`-propagating `runner.run` chains, but subsequent refactors (most notably `6fa9a5a refactor(remove-missing): route cmd_remove_missing through plan_remove_missing + RemoveMissingPlan::execute`) reshaped execution so the only remaining `runner.run` ended up inside the soft-warn helper -- leaving the variants orphaned.

Why this matters: an unreachable error variant with `#[from] CmdError` is a future-bug surface. A reader scanning the enum reasonably concludes that `runner.run` failures propagate raw `CmdError` into the parent error, and then writes a new direct-runner call that bypasses the soft-warn helper and silently produces a `Cmd` error that no caller expects. Removing the variants makes the soft-warn-only contract visible in the type.

Other `Cmd` variants in the crate (`PoolError`, `ProbeError`, `RecoverError`, `StatusError`, `EnrollKeyFileError`, `ScrubNeedsResumeError`, `ScrubResumeOrStartError`, `LockError`, `MountError`, `ScrubCancelError`, `DiscoverError`, `AddError`, `AckError`, `CloseMapperError`, `ReplaceError`, `LuksError`, `OwnershipError`) are live: they are auto-constructed via `#[from]` from `?`-propagated `runner.run` calls inside their modules. This refactor is intentionally scoped to the two modules where the variant is provably dead.

## Change

Delete the dead variants and their `#[from] CmdError` impls.

### `cli/src/remove_missing.rs:46-47`

```rust
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
```

Remove these two lines from the `RemoveMissingError` enum.

### `cli/src/remove.rs:45-46`

```rust
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
```

Remove these two lines from the `RemoveError` enum.

No other edits required. In particular:

- The soft-warn helpers (`check_relocation_space` in `remove_missing.rs:558-598`, `check_eviction_space`-equivalent in `remove.rs:693-706`) continue to convert `CmdError` to a `ProceedWithWarning` body via `format!("... {e} ...")` -- the `Display` impl on `CmdError` does not depend on the parent error's `From` impl.
- Both `RemoveError` and `RemoveMissingError` are entirely scoped to their own files (`grep -rln "RemoveMissingError\|RemoveError" cli/src/` returns only `remove.rs` and `remove_missing.rs`).
- `main.rs:429,455` only calls `.to_string()` on the error for `print_cli_error`, which routes through `Display`/`thiserror` and is unaffected by variant removal.
- No tests match on `RemoveMissingError::Cmd` or `RemoveError::Cmd` (`grep -rn` returns zero hits in `cli/src/` and `cli/tests/`).
- The `use crate::cmd::{CmdRequest, CommandRunner, Step};` import in `remove_missing.rs:2` (and the analogous one in `remove.rs`) stays -- those names are still used elsewhere in the file. `CmdError` is not imported at the top of either file (the variant references it via the full path `crate::cmd::CmdError`), so no import cleanup is needed.

## Critical files

- `cli/src/remove_missing.rs` -- delete lines 46-47.
- `cli/src/remove.rs` -- delete lines 45-46.

## Verification

1. `just test-rust` -- must pass. This is the primary signal: if any `?` chain or explicit construction was actually reaching the `Cmd` variant, compilation fails. The Rust unit tests also exercise the soft-warn `ProceedWithWarning` paths in both modules (search for `ENOSPC pre-flight check failed` test assertions in `remove.rs` and `remove_missing.rs`).
2. `cargo clippy --workspace --all-targets -- -D warnings` (or `just test-rust` if it already runs clippy) -- ensure no new `unused_imports` or similar warnings appear after the deletion.
3. `just test-vm braid-remove-disk braid-remove-missing-softwarn` -- runs the NixOS VM tests that exercise the live CLI paths end-to-end for both modules touched (`braid-remove-disk` covers `remove.rs`; `braid-remove-missing-softwarn` exercises the soft-warn helper in `remove_missing.rs`, the only `runner.run` callsite in that module). Not strictly required to catch this change (the variant is dead at compile time), but cheap insurance that nothing observable has shifted. Check names are exact attribute names in `flake.nix`; there are no bare `remove` / `remove-missing` attrs.

No fixture refresh needed (parser-critical tool versions are unchanged).
