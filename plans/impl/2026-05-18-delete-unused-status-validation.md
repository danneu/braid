# Delete unused `StatusError::Validation` variant

## Context

`StatusError::Validation(String)` is declared in `cli/src/status.rs:315-316`
but has zero call sites anywhere in the crate. It was previously constructed
by `disk_map::validate_config_name_stability(...).map_err(|e| StatusError::Validation(e.to_string()))`,
but commit `74feca5` ("move disk membership from nix config to cli-owned
runtime state", Mar 26) removed that call path during the membership refactor
and left the variant declaration behind. Classic refactor residue.

The `rustc` `dead_code` lint does not catch this because
`#[derive(thiserror::Error)]` synthesizes a `Display` impl that references
every variant, which keeps the variant "used" from the compiler's point of
view -- so the lint cannot flag it and only a manual `rg` reveals it.

Goal: delete the orphaned variant so contributors do not have to consider
it when adding `match` arms and reviewers do not have to explain its zero
call sites. No behavior change.

## Why this is a fix, not a wider refactor

The verify-issue sweep checked every other `thiserror` enum in `cli/src/`
for the same pattern. The other modules (`luks`, `add`, `remove`, `replace`,
`remove_missing`, `enroll_key_file`) each declare their own
`Validation(String)` variant and all of them are heavily constructed. This
is a single-site cleanup, not a wider dead-variant pattern.

## Blast radius

- `StatusError` is not imported anywhere outside `cli/src/status.rs`
  (`rg "use crate::status::"` shows only `BalanceReport`, `format_bytes`,
  `DiskReport`, `DiskStatus`, `ScrubReport`, `StatusCode`, `StatusReport`,
  `DiskErrors`, `resolve_alert_state`, `estimate_pool_capacity`,
  `get_balance_report` -- never `StatusError`).
- No `match` statement on `StatusError` anywhere -- tests use `matches!`
  macros and panic-on-other patterns for specific variants
  (`Membership(...)`, `Probe(...)`), which are non-exhaustive and unaffected.
- No comment, docstring, or test assertion references the `Validation`
  variant.

## The change

File: `cli/src/status.rs`

Delete these two lines from `pub enum StatusError` (currently L315-316):

```rust
    #[error("validation error: {0}")]
    Validation(String),
```

The remaining variants (`Probe`, `Cmd`, `Parse`, `Json`, `Membership`) stay
exactly as they are.

## Verification

- `cargo build -p braid-cli` -- compiles clean.
- `just test-rust` -- Rust unit tests still pass. No test references the
  variant, so no test edits are needed.
- `rg "StatusError::Validation" cli/src` -- returns nothing (sanity check that no
  stray references survive).

No VM tests are needed: the deletion cannot change runtime behavior because
the variant was never constructed.
