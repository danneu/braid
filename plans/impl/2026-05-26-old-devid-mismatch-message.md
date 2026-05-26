# Fix `OldDevidMismatch` message: drop Debug leak + false btrfs attribution

## Context

`braid replace --old <name> --missing-id <id>` cross-checks the operator's
`--missing-id` against the devid pool.json records for `--old`. When they
disagree, it returns `ReplaceError::OldDevidMismatch`, which `main.rs:658`
renders to the operator via `print_cli_error(&e.to_string())` (Display).

That message has two operator-facing defects:

1. **Debug leak.** The field is `pool_devid: Option<u64>` formatted with
   `{pool_devid:?}`, so it prints `Some(2)` -- a Rust Debug artifact that
   violates the project CLI output style. The variant is built at exactly one
   site (`replace.rs:1711`), always with `Some(persisted_devid)`; the
   `None` case is handled earlier by the separate `OldMemberMissingDevid`
   variant (`replace.rs:1686-1693`). So the `Option` is never `None` here.
2. **False attribution.** The message says `btrfs reports missing devid
   {observed}`, but `observed` is `supplied` = the operator's `--missing-id`.
   At the error site (`replace.rs:1710`) that value has not been checked
   against btrfs at all -- the `pool.missing_devids` check is downstream at
   `replace.rs:1722`. The two values being compared are operator input vs
   pool.json; neither is btrfs-reported.

A stale doc comment (`replace.rs:117-119`) compounds this: it claims the
variant also covers "the persisted devid is `None`" (it does not -- that's
`OldMemberMissingDevid`) and calls the supplied value the "resolved missing
devid," echoing the same mislabel. This stale comment is the likely origin of
the unnecessary `Option`.

The same `{:?}`-on-`Option<LuksUuid>` pattern exists at
`luks.rs:753` (`OwnershipError::Conflict`). It is internal-only (always
converted to `LuksError::MapperConflict` / `ProbeError::MapperConflict`, which
render correctly via a helper), so its leak is latent, but we fix it for
consistency since the correct rendering already exists one module over.

Intended outcome: the disagreement error reads accurately and carries no Debug
artifacts, and the codebase has no remaining `{:?}`-on-`Option` Display leak.

## Changes

### 1. `cli/src/replace.rs` -- `OldDevidMismatch` variant (lines 117-127)

Rewrite the doc comment, drop the `Option`, rename `observed` -> `supplied`,
and reword the Display (no `:?`, no false btrfs claim, plus a `braid status`
hint matching sibling variants):

```rust
    /// Operator's `--missing-id` disagrees with the old member's persisted
    /// `devid`: `--old` resolves to a member recording one devid while
    /// `--missing-id` names another. A typo guard caught before any btrfs
    /// cross-check. (The persisted-devid-is-`None` case is the separate
    /// `OldMemberMissingDevid` variant.)
    #[error(
        "--old '{old_name}' records devid {pool_devid} in pool.json, but --missing-id was {supplied}. --old and --missing-id disagree about which member is being replaced -- run 'braid status' to confirm which disk is missing."
    )]
    OldDevidMismatch {
        old_name: String,
        pool_devid: u64,
        supplied: u64,
    },
```

### 2. `cli/src/replace.rs` -- construction site (lines 1711-1715)

Drop the `Some(...)` wrapper; `supplied` is already the local binding name
(`if let Some(supplied) = missing_id`), so field shorthand applies:

```rust
            return Err(ReplaceError::OldDevidMismatch {
                old_name: old_name.as_str().to_owned(),
                pool_devid: persisted_devid,
                supplied,
            });
```

### 3. `cli/src/luks.rs` -- `OwnershipError::Conflict` Display (line 753)

Route through the existing `luks_found_display` helper (`luks.rs:149`),
mirroring how `LuksError::MapperConflict` does it at `luks.rs:113`:

```rust
    #[error("mapper conflict on '{name}': expected {expected}, found {}", luks_found_display(found))]
```

No field changes; `found: Option<LuksUuid>` stays (it is legitimately `None`
at two of its three construction sites).

## Tests

### Update existing assertions -- `cli/src/replace.rs:2712-2722`

In `missing_id_disagrees_with_persisted_devid`, rename the destructured field
and drop the `Some(...)`:

```rust
            ReplaceError::OldDevidMismatch {
                old_name,
                pool_devid,
                supplied,
            } => {
                assert_eq!(old_name, "disk2");
                assert_eq!(pool_devid, 2);
                assert_eq!(supplied, 99);
            }
```

### Add a Display-output regression guard -- same test

The bug was in the rendered string, so assert on it directly. These checks are
behavioral and structure-insensitive (they assert properties of operator
output, not field types). Render before the `match` consumes `err`:

```rust
        let msg = err.to_string();
        assert!(!msg.contains("Some("), "must not leak Debug Option wrapper: {msg}");
        assert!(!msg.contains("btrfs reports"), "must not attribute --missing-id to btrfs: {msg}");
        assert!(
            msg.contains("devid 2") && msg.contains("--missing-id was 99"),
            "should show persisted devid 2 and supplied 99: {msg}"
        );
```

No new test is needed for the `luks.rs` change: it is internal-only, no test
asserts on `OwnershipError::Conflict`'s rendered string (verified via grep),
and the public errors that carry the value (`LuksError::MapperConflict`,
`ProbeError::MapperConflict`) are already covered.

## Verification

- `just test-rust` -- runs the Rust unit tests (package `braid-cli`), including
  the updated `missing_id_disagrees_with_persisted_devid` and the luks
  ownership/conflict tests. This is the primary gate; the changed code path is
  exercised by `resolve_replace_source` unit tests.
- `cargo build` (or `cargo check`) -- confirms the `Option<u64>` -> `u64`
  change compiles with no other consumers (grep confirms the only `pool_devid`
  references are the Display, the field def, the one construction site, and the
  test).
- No VM tests required: this is a pure CLI error-rendering change with no
  systemd/mount/pool-lock blast radius.

## Out of scope (noted follow-up)

`luks_found_display` (`luks.rs:149`) and `found_display` (`probe.rs:115`) are
byte-identical. Unifying them into one shared helper is a reasonable cleanup
but is tangential to this bug and would touch `probe.rs`'s rendering path;
leave it for a separate change.

## Follow Up

- Unify byte-identical LUKS UUID rendering helpers in `cli/src/luks.rs` and `cli/src/probe.rs` in a separate cleanup, preserving the public `MapperConflict` output.
