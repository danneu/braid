# Plan: Pin bare-mode discover refusal messages in Rust unit tests

## Context

`braid discover` (no `--write`) refuses a healthy `ValidUuidKeyed` or
`Corrupt` `pool.json` with one of two operator-facing messages,
rendered inline in `cli/src/main.rs:912-929`. Today these messages are
only exercised by VM tests in `tests/cli/braid-discover.py`:

- The bare `Corrupt` message is asserted by the
  `assert_corrupt_preview_refuses` helper at `:47-58`, called from the
  off-schema-JSON subtest at `:108` and the unparseable subtest at
  `:117`, plus an independent re-assertion in the non-UUID-keyed
  subtest at `:120-135`.
- The bare `ValidUuidKeyed` message is asserted at `:186-190`.

No other test file in `tests/` references either substring, so any
wording drift takes a multi-minute VM run to surface.

Every adjacent fail-closed gate on the `--write` path lives in
`cli/src/discover.rs` as a `DiscoverWriteError` variant with
`thiserror`-rendered messages and a Rust unit test
(`discover_write_refuses_*` at `discover.rs:1760-2236`). The classifier
itself is also unit-tested (`classify_pool_json_returns_*` at
`discover.rs:2021-2128`). The bare-mode dispatch is the lone branch in
this neighborhood without a Rust test pinning its rendered string.

The wording has already drifted once before (`9c3acab` -> `93e2e42` ->
`f2da9a9` -> `cc2e946` per `git log -S "pool.json already exists at"`),
and decision 017 documents only the high-level "direct the user to
`braid add` or `braid discover --write`" intent -- the exact substring
is the kind of operator-facing invariant a string-pinning unit test is
designed to catch cheaply.

The fix lifts the bare-mode classification + message rendering into a
`discover.rs` helper backed by a thiserror enum, mirroring the existing
`--write` pattern, then pins each rendered message in a Rust unit
test. Both bare-mode branches (`ValidUuidKeyed` and `Corrupt`) move
together -- they share the same VM-only coverage gap and the same
inline-in-`main.rs` structure, so a fix that covered only one would
just shift the asymmetry by one slot.

User-facing wording is byte-identical to today; this is a non-behavioral
refactor whose only product is testability.

## Approach

### 1. New error enum in `cli/src/discover.rs`

Add a `BareDiscoverError` thiserror enum next to the existing
`DiscoverWriteError` (around `discover.rs:166`). Two variants, exact
wording lifted from `main.rs:917` and `main.rs:924`:

```rust
/// Bare `braid discover` (no --write) preflight refusals. Mirrors
/// `DiscoverWriteError` so both gating paths have a single thiserror
/// surface and matching unit-test coverage in this module.
#[derive(Debug, thiserror::Error)]
pub enum BareDiscoverError {
    #[error(
        "pool.json already exists at {path} -- use 'braid add' to add disks"
    )]
    ValidUuidKeyed { path: String },
    #[error(
        "pool.json at {path} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)"
    )]
    Corrupt { path: String },
}
```

### 2. New gating helper in `cli/src/discover.rs`

Add `pub fn check_pool_json_for_bare_discover(path: &Path) -> Result<(), BareDiscoverError>`
next to `write_discovered_membership` (around `discover.rs:562`). It
calls the existing `classify_pool_json(path)` at `discover.rs:238` and
maps:

- `Missing` -> `Ok(())`
- `ValidUuidKeyed` -> `Err(BareDiscoverError::ValidUuidKeyed { path: path.display().to_string() })`
- `Corrupt` -> `Err(BareDiscoverError::Corrupt { path: path.display().to_string() })`

No new I/O -- the classifier already does the load.

Per AGENTS.md, the new `pub fn` carries a one-to-three-line `///` doc
comment justifying the boundary -- e.g. "Centralizes bare `braid
discover` state-file gating so the CLI dispatch in `main.rs` and the
wording-pinning unit tests in this module share one refusal-message
surface, mirroring `write_discovered_membership` for the `--write`
path." The enum-level doc on `BareDiscoverError` already covers the
variants per AGENTS.md's "enum variants already covered by an enum-level
doc" skip rule, so no per-variant doc comments are required.

### 3. Replace inline match in `cli/src/main.rs`

Replace the inline `match braid_cli::discover::classify_pool_json(&pool_json)`
block at `main.rs:911-930` with a single helper call:

```rust
if !args.write {
    if let Err(e) = braid_cli::discover::check_pool_json_for_bare_discover(&pool_json) {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
}
```

`print_cli_error` (`cli/src/main.rs:1251-1257`) still owns the
`error: ` prefix; the helper renders only the message body, matching
how `DiscoverWriteError` is rendered today via the `match write_discovered_membership(...)` arm at `main.rs:947-960`.

### 4. New unit tests in `cli/src/discover.rs` test module

Place alongside the existing `discover_write_refuses_*` tests
(`discover.rs:1760-2236`). Each test gets the literal `// Intent` /
`// Why it exists` / `// Scenario` line-comment preamble directly above
`#[test]` per `docs/testing.md`. Three tests:

- `check_pool_json_for_bare_discover_refuses_valid_uuid_keyed` --
  seed a UUID-keyed `pool.json` via `save_membership` (same setup as
  `discover_write_refuses_when_pool_json_is_valid_uuid_keyed` at
  `:1838`), call the helper, then:
    - `assert!(matches!(&err, BareDiscoverError::ValidUuidKeyed { .. }))`
    - `assert_eq!(err.to_string(), format!("pool.json already exists at {} -- use 'braid add' to add disks", paths.pool_json().display()))`

  The `matches!` scrutinee must borrow (`&err`) so `err.to_string()`
  on the next line still compiles -- mirrors `discover.rs:1869`.
  Suggested preamble: Intent -- bare `braid discover` refuses a healthy
  UUID-keyed `pool.json` with the byte-exact `use 'braid add'`
  remediation. Why -- every byte of the refusal is operator-facing
  contract; this is the cheap regression net for wording drift that
  decision 017 leaves to code-level pinning. Scenario -- an operator
  who knows their pool.json is fine reflexively runs `braid discover`
  to "refresh" and expects to be told to use `braid add` instead.

- `check_pool_json_for_bare_discover_refuses_corrupt` -- write
  `"not-json"` to `paths.pool_json()` (matches
  `classify_pool_json_returns_corrupt_for_unparseable` at `:2064`),
  call the helper, then:
    - `assert!(matches!(&err, BareDiscoverError::Corrupt { .. }))`
    - `assert_eq!(err.to_string(), format!("pool.json at {} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)", paths.pool_json().display()))`

  Same `&err` borrow rule as the ValidUuidKeyed test above.
  Suggested preamble: Intent -- corrupt `pool.json` surfaces the
  byte-exact rebuild remediation through the bare path. Why -- same
  wording-drift net as the ValidUuidKeyed test, applied to the Corrupt
  sibling gate so the asymmetry does not shift by one slot. Scenario --
  power loss truncates `pool.json` to non-JSON bytes; the operator
  runs bare `braid discover` and must be directed at `braid discover
  --write`.

- `check_pool_json_for_bare_discover_passes_when_missing` -- empty
  `StatePaths`, call the helper, `assert!(check_pool_json_for_bare_discover(&paths.pool_json()).is_ok())`.
  Suggested preamble: Intent -- absent `pool.json` is not a refusal.
  Why -- pins the `Missing -> Ok(())` arm so the gate cannot silently
  flip to fail-closed-by-default and break first-boot. Scenario -- a
  freshly installed NAS runs `braid discover` for the first time with
  no prior state file.

Use the existing `StatePaths::custom(...)` + `tempfile::tempdir`
fixture pattern already used throughout the `--write` tests
(`discover.rs:1762`, `:1795`, `:1840`, ...). The `assert_eq!` form
pins the rendered string byte-for-byte; the tempdir-dependent prefix
is folded in via `paths.pool_json().display()` in the expected
`format!()`. This is a deliberate divergence from the existing
`--write` tests' `assert!(msg.contains(...))` style (`:1776`, `:1873`,
`:2187`) -- the looser form would still pass if most of the
remediation text drifted, which is precisely the regression this plan
exists to catch.

## Files to modify

- `cli/src/discover.rs` -- add `BareDiscoverError` enum, add
  `check_pool_json_for_bare_discover` helper, add three unit tests.
- `cli/src/main.rs` -- replace the inline bare-mode match at
  `:911-930` with a single helper call.

No changes to VM tests (`tests/cli/braid-discover.py`), decision docs,
`print_cli_error`, or `classify_pool_json`. No new dependencies.

## Verification

1. `just test-rust` -- the three new unit tests pass; existing
   `classify_pool_json_*` and `discover_write_refuses_*` tests are
   untouched and still pass. The two new refusal tests' `assert_eq!`
   against `format!()`-built expected strings are themselves the
   structural pin: any wording drift on either message fails a Rust
   test in seconds.
2. `just test-vm braid-discover` -- all bare-mode VM subtests keep
   passing on byte-identical wording, confirming the refactor is
   non-behavioral end-to-end. Concretely: the
   `assert_corrupt_preview_refuses` helper at
   `tests/cli/braid-discover.py:47-58` (called from the off-schema and
   unparseable subtests at `:108` and `:117`), the non-UUID-keyed
   subtest's substring assertion at `:120-135`, and the
   ValidUuidKeyed bare-refusal subtest at `:186-190`.
