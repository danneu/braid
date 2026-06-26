# Plan: dedup `fallback_disk_luks_lock` via a verdict-only helper

## Context

`cli/src/tui/probe.rs#fallback_disk_luks_lock` is the best-effort
LUKS lock classifier for a declared disk the mounted-pool probe could not
identify by UUID or devid. It returns `(DiskLockState, Option<String>)` where the
second element is the observed backing path ("underlying").

A simplicity finding flagged that the post-`underlying` arms rebuild the same
`(DiskLockState::Unknown, Some(underlying.as_str().to_owned()))` tuple over and
over. The investigation confirmed the duplication is real and **worse than the
finding stated**: 6 `return` sites plus the tail `else` (7 total) construct
`(Unknown, Some(underlying))`, not the "four" the finding claimed. The logic is
correct today; the cost is maintenance -- a future change to the
"observed-but-unverified" representation must be made in 7 places, and an arm that
forgot to carry `underlying` would silently change the model's `underlying_present`
field. Only the path-mismatch arm has a test pinning that carry
(`cli/src/tui/probe.rs#probe_fallback_backing_path_mismatch_does_not_read_foreign_metadata`);
the other five `Unknown` arms are unpinned.

**Key insight that shapes the fix:** once a backing device is observed, the
carried path is *invariant* -- every remaining outcome pairs `Some(underlying)`
with a verdict that is `Unlocked` iff fully ownership-verified, else `Unknown`.
Only the `DiskLockState` varies. So the ideal fix makes the pairing structural:
extract the verdict computation into a helper that returns a bare
`DiskLockState`, and pair it with `Some(underlying)` exactly once at the tail.

This is a **pivot** from the finding's proposed fix (hoist the string into one
`let observed` and `.clone()` it into each arm). The finding's version only
de-duplicates the *string*; it still writes the `(Unknown, observed.clone())`
*tuple* at all 7 arms, so the "an arm forgot to carry the path" failure mode it
worries about survives. Returning a bare verdict removes that failure mode
entirely.

## Approach (recommended)

Split `fallback_disk_luks_lock` into a thin outer function (handles the
pre-`underlying` states) and a new private helper `classify_open_mapper_lock`
(computes the verdict for an already-open mapper, returns `DiskLockState`).

Behavior is preserved **byte-for-byte**: the same cryptsetup commands run in the
same order, every input maps to the identical `(DiskLockState, Option<String>)`,
and the carried path is the raw `underlying` from `cryptsetup status` (not the
canonicalized form) exactly as today.

### Outer function (unchanged pre-`underlying` arms; single tail pairing)

```rust
fn fallback_disk_luks_lock<R: CommandRunner>(
    runner: &R,
    disk_name: &DiskName,
    by_id_path: &str,
    expected_uuid: Option<&LuksUuid>,
    backing_path_resolver: &dyn BackingPathResolver,
) -> (DiskLockState, Option<String>) {
    let status_raw = match runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper_name(disk_name),
    }) {
        Ok(raw) => raw,
        Err(_) => return (DiskLockState::Unknown, None),
    };
    let underlying = match parse_cryptsetup_status(&status_raw) {
        Ok(CryptsetupStatusOutput::Inactive) => return (DiskLockState::Locked, None),
        Ok(CryptsetupStatusOutput::Active { backing: BackingDevice::Path(path) }) => path,
        Ok(CryptsetupStatusOutput::Active { backing: BackingDevice::Null }) => {
            return (DiskLockState::Unknown, None);
        }
        Err(_) => return (DiskLockState::Unknown, None),
    };

    // Once a backing device is observed the carried path is invariant; only the
    // verdict varies. Pair it with Some(underlying) exactly once here.
    let lock = classify_open_mapper_lock(
        runner,
        by_id_path,
        underlying.as_str(),
        expected_uuid,
        backing_path_resolver,
    );
    (lock, Some(underlying.as_str().to_owned()))
}
```

### New helper (verdict only; flattened with `let-else`)

```rust
/// Verdict for an already-open braid mapper whose backing device is known.
/// Returns `Unlocked` only when both the backing path and the LUKS UUID match
/// the configured disk; any probe/parse failure or mismatch collapses to
/// `Unknown`. Never returns `Locked` -- inactivity is resolved by the caller
/// before a backing device exists. Silent by design (it does spawn
/// `cryptsetup luksUUID`, so not pure): unlike the close-time
/// `probe_observed_mapper_uuid`, it emits no operator `Warning:` lines (no
/// `emit_status`), so the TUI render path stays uncorrupted.
fn classify_open_mapper_lock<R: CommandRunner>(
    runner: &R,
    by_id_path: &str,
    observed: &str,
    expected_uuid: Option<&LuksUuid>,
    backing_path_resolver: &dyn BackingPathResolver,
) -> DiskLockState {
    let Ok(expected_path) = backing_path_resolver.canonicalize(by_id_path) else {
        return DiskLockState::Unknown;
    };
    let Ok(found_path) = backing_path_resolver.canonicalize(observed) else {
        return DiskLockState::Unknown;
    };
    if expected_path != found_path {
        return DiskLockState::Unknown;
    }
    let Some(expected_uuid) = expected_uuid else {
        return DiskLockState::Unknown;
    };
    let Ok(uuid_raw) = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: observed.to_owned(),
    }) else {
        return DiskLockState::Unknown;
    };
    let Ok(found) = parse_cryptsetup_luks_uuid(&uuid_raw) else {
        return DiskLockState::Unknown;
    };
    if &found.uuid == expected_uuid {
        DiskLockState::Unlocked
    } else {
        DiskLockState::Unknown
    }
}
```

This drops the construction sites for `(Unknown, Some(underlying))` from 7 to 0
(the verdict arms return a bare enum), and constructs the carried `Some(...)`
exactly once. `let-else` matches the file's existing idiom (already used 12x in
`probe.rs`, including the current `let Some(expected_uuid) = expected_uuid else`
arm in `fallback_disk_luks_lock`). The command order (`status` -> path
canonicalize x2 -> `luksUUID`) and the early-return-before-`luksUUID` on path
mismatch are unchanged.

## Critical files

- `cli/src/tui/probe.rs` -- the only file changed. Replace
  `fallback_disk_luks_lock` with the outer function above and add the
  `classify_open_mapper_lock` helper beside it. No call-site changes:
  `build_disk_luks_states` still receives the identical tuple.

No changes to `model.rs`, `luks.rs`, or any consumer. No new imports
(`CmdRequest`, `parse_cryptsetup_luks_uuid`, `DiskLockState`, `LuksUuid`,
`BackingPathResolver`, `CommandRunner` are already in scope).

## Tests

No new tests required; this is a structure-preserving refactor and the existing
suite already covers the behavior and pins the invariant being made structural:

- `cli/src/tui/probe.rs#probe_fallback_backing_path_mismatch_does_not_read_foreign_metadata`
  -- asserts `lock == Unknown` **and** `underlying_present == Some("/dev/vdz")`:
  the "carry survives on an Unknown arm" guard. (Relies on
  `cli/src/test_fixtures/shared.rs#MockBackingPathResolver` passing unregistered
  paths through identity, so `/dev/vdb != /dev/vdz` triggers the mismatch arm
  before any `luksUUID` call.)
- `cli/src/tui/probe.rs#probe_fallback_classifies_foreign_uuid_mapper_as_unknown`
  -- the valid-but-wrong-UUID tail `else` -> `Unknown`.
- `cli/src/tui/probe.rs#probe_classifies_unmounted_open_and_closed_mappers`
  -- open/Unlocked + Locked(inactive) coverage with `underlying_present` asserted
  `Some`/`None` respectively.
- `cli/src/tui/probe.rs#probe_status_active_metadata_failed_decouples_lock_and_metadata`
  -- Unlocked verdict independent of metadata.

The five other `Unknown` arms (both canonicalize failures, no-`expected_uuid`,
`luksUUID` cmd failure, `luksUUID` parse failure) were untested before and remain
so. Adding per-arm tests is optional and explicitly out of scope: the refactor
makes the carried path impossible to drop on any of them (it is the single tail
`Some(...)`), which is strictly stronger than test coverage of each arm. Per the
project's plan-review bar, do not add structure-sensitive tests that just pin the
new internal split.

## Considered and rejected

- **Reuse `luks.rs#classify_mapper_ownership`** (the strict planner/executor
  classifier). Rejected: incompatible contract. It returns
  `Result<MapperOwnership, OwnershipError>` and (a) does not expose the raw
  observed `underlying` the TUI must carry out -- its `BackingPathMismatch` error
  carries the *canonicalized* `found_path`, which would change the path string
  the model surfaces and break the `Some("/dev/vdz")` assertion; (b) surfaces ~5
  typed errors the TUI would have to fold back to `Unknown`; (c) requires an
  expected UUID (thunk), whereas the TUI handles `expected_uuid: None` -> `Unknown`.
  Forcing reuse would be more complex and behavior-changing.
- **Reuse `probe_mapper_uuid.rs#probe_observed_mapper_uuid`** (the best-effort
  close-time classifier, `MapperOwnership{Owned,Inactive,Unverified}`). Rejected:
  it emits operator `Warning:` lines via `emit_status` (fatal for a silent TUI
  render surface), skips the backing-path comparison the TUI depends on, and does
  not carry out the underlying path. The codebase already maintains these as
  three deliberately distinct policies (two separate `MapperOwnership` enums); the
  pivot respects that boundary rather than collapsing it behind config flags.
- **Introduce a named struct for `(DiskLockState, Option<String>)`** across
  `build_disk_luks_states` / `mounted_classification` / signatures. Deferred, not
  rejected: a legitimate readability improvement but a larger, separable change
  touching multiple sites and test helpers. Out of scope for this focused pivot;
  worth a follow-up if desired.

## Verification

1. `just test-rust` -- runs `cargo test --lib --bin braid ...`; all
   `probe.rs` fallback tests above must pass unchanged.
   - Focused run while iterating:
     `cargo test --manifest-path cli/Cargo.toml --lib probe_fallback`
     and `... --lib probe_status_active`.
2. `just clippy` -- `cargo clippy --manifest-path cli/Cargo.toml --tests`; expect
   no new warnings (the `let-else` form is clippy-clean and idiomatic here).
3. `just check-output-ascii` -- no new echo/user-facing strings added, so this
   stays green; included only as a guard since the file is in its scan set.
4. Confirm zero diff in behavior by eye: every arm of the old function maps to an
   arm of the new pair producing the identical `(DiskLockState, Option<String>)`.
