# Plan: Pin replace mapper-open UUID-conflict abort with a seed 635 unit test

## Context

`verify_existing_luks_open_mapper_target` (`cli/src/replace.rs:998-1042`)
is the open-boundary defense-in-depth gate that fires when `braid replace`
finds the new target's mapper already open at execute time
(`cli/src/replace.rs:767-781`). It delegates to
`classify_mapper_ownership` (`cli/src/luks.rs:836`) and translates each
of four `OwnershipError` arms into a distinct `ReplaceError` variant:

| `OwnershipError`              | `ReplaceError`                          | Test       |
| ----------------------------- | --------------------------------------- | ---------- |
| `BackingPathMismatch`         | `NewTargetMapperBackingMismatch`        | seed 632 (5462) |
| `BackingPathResolveError`     | `NewTargetMapperBackingResolveError`    | seed 634 (5561) |
| (success / `Owned`)           | (Ok)                                    | seed 633 (5524) |
| **`Conflict`**                | **`NewTargetUuidMismatchAtOpen`**       | **MISSING** |

Three of four mappings have direct unit tests. The fourth -- the
UUID-conflict arm -- is reachable in production via the call site at
`cli/src/replace.rs:774` but has no test that exercises the
`replace.rs:1032-1038` mapping arm. The symmetric closed-mapper path
(`probe_existing_luks_new_target_uuid`, `cli/src/replace.rs:967-994`) is
covered at seed 630 (`replace_existing_luks_open_boundary_probe_mismatch_aborts`,
line 5373); the mapper-open side should have a parallel pin.

The underlying classifier behavior is pinned at
`cli/src/luks.rs:2261` (`ensure_luks_open_active_mapper_different_uuid_conflicts`),
so the production safety guarantee already holds. What is missing is the
replace-side error-mapping unit test: a refactor that collapsed
`OwnershipError::Conflict` into `Validation` or
`NewTargetMapperBackingMismatch` would not be caught by any current test
in `replace.rs`.

The cloned-header VM test
(`tests/cli/replace-cloned-luks-header-rejected.py`) exercises the
`BackingPathMismatch` arm at integration level. It does not cover the
matched-path / mismatched-UUID arm because that fact pattern cannot
occur with a cloned header (cloned headers duplicate the UUID by
construction). The Conflict arm only fires under a different operator
mishap: a foreign disk is open under the target mapper name AND its
by-id link happens to canonicalize to the same kernel device as the
configured target. The arm's contract should be pinned directly.

Intended outcome: one new unit test, named with the existing
`replace_existing_luks_open_mapper_backing_*` convention, that fails if
the `OwnershipError::Conflict -> ReplaceError::NewTargetUuidMismatchAtOpen`
mapping at `replace.rs:1032-1038` is broken. No production code change.

## Approach

Add seed 635 to the `#[cfg(test)] mod tests` block in
`cli/src/replace.rs`, immediately after seed 634 (i.e. after the closing
brace of `replace_existing_luks_open_mapper_backing_resolve_error_aborts`,
currently ending near line 5610). Mirror seed 632's structure exactly --
same helpers, same assertion shape -- but swap two inputs and one
expected variant:

1. Configure `MockBackingPathResolver` so both `/dev/disk/by-id/Y` AND
   the live backing device (`/dev/vdf`) canonicalize to the same string
   (`"/dev/vdf"`). This makes the path check in
   `classify_mapper_ownership` pass, so the UUID check becomes the
   failure point.
   - `MockBackingPathResolver::default().with_path("/dev/disk/by-id/Y", "/dev/vdf")`
     handles the by-id side.
   - The identity default (`None => Ok(path.to_owned())`, confirmed at
     `cli/src/test_fixtures/shared.rs:236-241`) handles the backing
     side -- no explicit override needed for `/dev/vdf`.
2. Seed the runner with `runner_with_active_mapper_uuid` (existing
   helper at `cli/src/replace.rs:5248`) so that:
   - `CryptsetupStatus { mapper: "braid-disk3" }` reports backing
     `/dev/vdf`.
   - `CryptsetupLuksUuid { device: "/dev/vdf" }` returns `U_FOREIGN`
     (different from the expected `U_NEW`).
3. Assert the error is
   `ReplaceError::NewTargetUuidMismatchAtOpen { by_id, expected: U_NEW,
   observed: U_FOREIGN.as_str() }` (no `BackingPathMismatch`, no
   `BackingPathResolveError`).
4. Assert post-conditions matching seed 632:
   - No `CryptsetupLuksOpen` issued.
   - No `BtrfsReplaceStart` issued.

The function name follows the cluster convention
(`replace_existing_luks_open_mapper_backing_*_aborts`):
**`replace_existing_luks_open_mapper_backing_uuid_mismatch_aborts`**.

### Critical files to modify

- `cli/src/replace.rs` -- one new `#[test]` inside the existing
  `mod tests` block, placed after seed 634. No other file changes.

### Existing helpers reused

- `runner_with_active_mapper_uuid` (`cli/src/replace.rs:5248`) -- seeds
  `CryptsetupStatus` + `CryptsetupLuksUuid` for an active mapper.
  Already used by seeds 620/621/632/633.
- `MockBackingPathResolver` (`cli/src/test_fixtures/shared.rs:236-241`)
  -- identity-by-default with `.with_path()` overrides. Already used
  by seeds 632/633/634.
- `LuksUuid::parse`, `ByIdPath::parse`, `MapperName`, `RawCommandOutput`
  -- all imported in the existing tests at 5462/5524/5561.
- `verify_existing_luks_open_mapper_target` (`cli/src/replace.rs:998`)
  -- the function under test, already called by seeds 632/633/634.

### Test preamble convention (matched verbatim from seed 632)

```rust
/// Seed 635: already-open ExistingLuks replace target UUID-mismatch
/// arm. When the open mapper's backing kernel path canonicalizes to
/// the configured by-id target but the backing device's LUKS UUID
/// disagrees with the journaled `new_uuid`, the classifier returns
/// `OwnershipError::Conflict` and `verify_existing_luks_open_mapper_target`
/// maps it to `NewTargetUuidMismatchAtOpen` before any
/// `BtrfsReplaceStart`.
//
// Intent: verify_existing_luks_open_mapper_target maps
//   OwnershipError::Conflict to NewTargetUuidMismatchAtOpen on the
//   mapper_open=true path, with no replace mutation issued.
// Why: pins the only untested arm of the 4-arm OwnershipError ->
//   ReplaceError map at replace.rs:1015-1041; a refactor that
//   collapses Conflict into Validation or BackingPathMismatch would
//   otherwise pass.
// Scenario: /dev/disk/by-id/Y and the live backing /dev/vdf both
//   canonicalize to /dev/vdf (path check passes), but
//   cryptsetup luksUUID /dev/vdf returns U_FOREIGN != U_NEW.
#[test]
fn replace_existing_luks_open_mapper_backing_uuid_mismatch_aborts() {
    // body sketched below
}
```

### Test body sketch

```rust
let u_new = LuksUuid::parse("...uniquely-suffixed-with-0635").unwrap();
let u_foreign = LuksUuid::parse("...different-uuid").unwrap();
let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();

let runner = runner_with_active_mapper_uuid(
    "braid-disk3",
    "/dev/vdf",
    RawCommandOutput {
        cmd: "cryptsetup luksUUID /dev/vdf".into(),
        stdout: format!("{u_foreign}\n"),
        stderr: String::new(),
        exit_status: 0,
    },
);
let resolver = MockBackingPathResolver::default()
    .with_path("/dev/disk/by-id/Y", "/dev/vdf");

let err = verify_existing_luks_open_mapper_target(
    &runner,
    "disk3",
    &MapperName("braid-disk3".into()),
    &by_id,
    &u_new,
    &resolver,
)
.unwrap_err();

match err {
    ReplaceError::NewTargetUuidMismatchAtOpen {
        by_id: err_by_id,
        expected,
        observed,
    } => {
        assert_eq!(err_by_id, by_id);
        assert_eq!(expected, u_new);
        assert_eq!(observed, u_foreign.as_str().to_owned());
    }
    other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
}

let requests = runner.requests();
assert!(
    !requests
        .iter()
        .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
    "no CryptsetupLuksOpen may issue on the UUID-mismatch path"
);
assert!(
    !requests
        .iter()
        .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
    "no BtrfsReplaceStart may issue on the UUID-mismatch path"
);
```

UUIDs follow the existing convention of suffixing the seed number into
the last group (e.g. seeds 632/633 use `...0632` and `...0633`); pick
`...0635` for `u_new` and `...0636` for `u_foreign` (a free pair
within the cluster), or any unused pattern -- the exact bytes don't
matter as long as they parse and differ.

## Out of scope (intentionally dropped)

- **The three `--old == --new` tests** (3077, 4635, 4977). The
  upstream finding flags them as near-duplicates; on inspection each
  pins a distinct seam (cmd-level + inhibitor/journal vs. plan-only
  no-probe vs. cmd-level no-probe + stderr) and the simplification is
  minor with non-trivial pinning risk. Leave them alone.
- **A new VM test.** The cloned-header VM test already covers the
  end-to-end mapper-open defense at the `BackingPathMismatch` arm;
  the matched-path / mismatched-UUID arm is so narrow at the operator
  level that a unit test is the right granularity.
- **Adding a Test Plan section reference.** Seeds 632-634 do not have
  "Pinned by Test Plan section ..." footers; only seeds 600 and 650
  do. Seed 635 follows the local cluster style and omits it.
- **Touching `probe_observed_mapper_uuid`** or any other
  defense-in-depth gate beyond `verify_existing_luks_open_mapper_target`.

## Verification

1. **`just test-rust`** -- the new `#[test]` must pass. To prove the
   test actually exercises the intended arm (and not, say, a different
   error variant), confirm during implementation by transiently
   weakening the mapping at `replace.rs:1032-1038` (e.g. return
   `ReplaceError::Validation(...)` instead of
   `NewTargetUuidMismatchAtOpen`) and verifying the new test fails
   while the existing seed 632/633/634 tests stay green. Revert before
   committing.
2. **No production code change** -- diff should be confined to a
   single `#[test]` block inside `cli/src/replace.rs`'s `mod tests`.
3. **No fixture refresh, no doc updates.** The test exercises in-tree
   mocks only; it does not touch parser fixtures, NixOS modules,
   reference sources, or any of the principles/decisions docs.
4. **Spot-check `grep -n '/// Seed 635' cli/src/replace.rs`** returns
   exactly one match, located between seed 634 (line 5551) and seed
   640 (line 5612).
