# Document `target_underlying` as a display-only plan-time snapshot

## Context

A code-review finding (Low / Simplicity) flagged `RemoveWorkPlan.target_underlying`
in `cli/src/remove.rs` as a non-obvious extra field: a bare `String` snapshot
threaded through the whole work plan whose only consumer is a best-effort lsblk
lookup for the confirm prompt's hardware line. Future readers have to trace the
field to learn it is display-only.

Verification confirmed the finding's facts but rejected its "drop and re-resolve
at confirm time" alternative: `execute()` deliberately holds no `PoolState`
(planning drops it; execute re-validates via `validate_pool_topology`), so
re-resolving would mean a redundant `probe_pool` purely for a cosmetic line, and
it would break the symmetry of the struct's four other plan-time snapshot fields.
The field is correct and architecturally consistent -- it just lacks the `///`
that four of its five sibling snapshot fields already carry. The intended outcome
is to dissolve the maintenance cost (and pre-empt the next reviewer filing the
same finding) with a doc comment, changing no behavior.

## The change

One edit, one file. Add a doc comment immediately above `target_underlying`
(currently `cli/src/remove.rs:141`, between the `target_mapper` field and
`remaining`), matching the density/style of the `target_mapper` and
`expected_present_identities` comments that bracket it:

```rust
    /// Live backing path of the target captured at planning time -- the
    /// cryptsetup-reported `PoolDevice.underlying`, not the mapper path or
    /// a by-id handle (decision 024: present-device probes use live paths).
    /// Display-only: its sole consumer is the best-effort confirm-prompt
    /// hardware line (`confirm::query_disk_hw_info`) behind the interactive
    /// `!params.yes` gate; no mutation, journal, or validation reads it.
    /// Snapshotted rather than re-resolved because `execute()` holds no
    /// `PoolState` to call `underlying_for_uuid` on (contrast replace.rs).
    target_underlying: String,
```

The comment answers the three questions a reader must trace today: which path it
is (live cryptsetup backing path, not mapper/by-id, per decision 024), who reads
it (one cosmetic best-effort consumer on the interactive path), and why it is a
snapshot rather than a live re-resolve (`execute()` has no `PoolState`).

## Scope notes (considered, not doing)

- **Not** drop + re-probe in `execute()`: redundant `probe_pool` spawns for a
  cosmetic line; breaks snapshot symmetry with `target_uuid` / `target_devid` /
  `target_mapper` / `expected_present_identities`. `replace.rs` resolves live
  only because its `execute()` still holds `pool`; remove's plan/execute split
  (ADR 022) deliberately does not.
- **Not** store a `DiskHwInfo` instead of the path: would run lsblk at plan time
  on `--dry-run` and `--yes` runs where the hw line is never shown.
- **Not** a `BackingPath` newtype: backing paths are bare `String`/`&str`
  everywhere; newtyping one site is inconsistent and unrelated to the finding.
- **Leave `target_devid` undocumented**: its `Devid` type and three obvious
  consumers (capacity check, confirm line, ack cleanup) are self-documenting.

## Critical files

- `cli/src/remove.rs` -- the only file changed (doc comment on the
  `target_underlying` field of `RemoveWorkPlan`).

## Tests

None. Comment-only, no behavioral delta. The claim the comment makes about
existing behavior is already pinned by
`cmd_remove_confirm_hw_line_resolves_from_live_backing_path`
(`cli/src/remove.rs`), which registers lsblk hw only on `/dev/vdc` and asserts
the model + serial appear -- locking the hw line to the live backing path. A test
asserting "exactly one consumer" would be structure-sensitive and brittle, so it
is out of scope.

## Verification

- `just test-rust` (or `cargo build -p braid-cli` + `just clippy`, i.e.
  `cargo clippy --manifest-path cli/Cargo.toml --tests`) -- confirms the comment
  breaks neither the doc-comment lint nor the build.
- Existing pinning test stays green by construction (no code path touched).
