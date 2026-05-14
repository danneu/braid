# Plan: path-and-UUID ownership at the mapper boundary

## Context

A code-review finding flagged `cli/src/replace.rs:731-738` (the
`ExistingLuks { mapper_open: true }` arm of `braid replace`): the arm
skips the open-boundary re-probe that the closed-mapper sibling at
`:706-722` performs, so `btrfs replace start` can be fed a mapper bound
to a foreign disk. The worst-case scenario the finding cites is a
**cloned LUKS header**: operator pre-opens `braid-<new_name>` against
a foreign disk whose UUID was copied from `new_by_id`'s disk;
`assert_new_uuid_unique` (`replace.rs:1309-1328`) only inspects
`pool.devices`+membership, so the cloned mapper slips through and
`btrfs replace start` writes pool data into the foreign disk.
Decision-024 (`docs/decisions/024-luks-uuid-identity.md:124-128`) is
explicit that cloned LUKS headers must be rejected as duplicate
identity.

The finding's proposed fix -- a UUID re-probe of the mapper's backing
via `probe_observed_mapper_uuid` -- **does not close the cloned-header
hole**: both disks share UUID X by construction of the clone, so the
re-probe matches and the gate still passes. Verified by reading
`cli/src/probe_mapper_uuid.rs:30-117`.

The root cause is in `classify_mapper_ownership` at
`cli/src/luks.rs:748-803`: it labels a mapper as `Owned` based purely
on UUID equivalence between the mapper's backing and the expected
by-id. `cryptsetup luksOpen <device> <mapper>` binds dm-crypt to a
specific kernel block device with no UUID-mediated redirection, so the
authoritative ownership check is a **block-device path comparison**,
not a UUID comparison.

This pivots the finding's fix: tighten the central ownership classifier
to require both `canonicalize(mapper.backing_device) ==
canonicalize(expected_by_id)` and the existing UUID match. The fix
applies uniformly to planning (`probe_config_disk`) and execute-time
short-circuits (`ensure_luks_open` / `ensure_luks_open_with_key_file`),
which closes the same cloned-header trap in `braid add`'s already-open
arm (`add.rs:1832`) as a byproduct.

## Approach

1. **Tighten the central seam.** Add a backing-path comparison to
   `classify_mapper_ownership`. This is the only function that decides
   "is this mapper ours?", and exactly three callers consume it, all
   already holding a `&ByIdPath`.

2. **Plumb a narrow resolver trait.** Introduce a `BackingPathResolver`
   trait (one method: `canonicalize(&str) -> Result<String, io::Error>`)
   local to `luks.rs`. Production impl wraps `std::fs::canonicalize`.
   Test mocks default to identity-by-input. Smallest blast radius
   compared to lifting `canonicalize` onto `Filesystem` (~16 impls) or
   widening `ByIdResolver` (whose list-entries semantics most callers
   shouldn't depend on).

3. **Two distinct error variants for the new arm.** Path equality has
   two failure modes with different remediation, so model them
   separately:
   - `BackingPathMismatch { name, expected_path, found_path }` --
     both paths resolved cleanly but point to different physical
     disks. Remediation: "close the conflicting mapper". This is the
     cloned-header case.
   - `BackingPathResolveError { name, by_id, source }` -- one of the
     two `canonicalize` calls returned an `io::Error` (vanished by-id,
     stale udev, EACCES). Remediation: "check that the configured
     disk is plugged in and that udev has populated
     `/dev/disk/by-id/`". Surfaces verbatim through
     `LuksError::MapperBackingResolveError`,
     `ProbeError::MapperBackingResolveError`, and
     `ReplaceError::NewTargetMapperBackingResolveError`.

   Do **not** reuse the existing UUID-mismatch variants -- their
   remediation text says "detach the foreign disk and retry"; the
   backing-path cases want different fixes. Reserve
   `BackingPathMismatch` strictly for "both resolved, paths differ" so
   the unit test does not cement the misleading
   "canonicalize-failed-as-mismatch" wording.

4. **Restore the open-boundary symmetry in replace.** Replace the
   skip-comment at `cli/src/replace.rs:731-738` with a fresh
   `classify_mapper_ownership` call right before `pool_replace_device`
   (`:788`). This is the execute-time defense-in-depth mirror of the
   closed-mapper arm's `probe_existing_luks_new_target_uuid` at `:721`,
   and closes the plan-to-execute drift window for the open-mapper case.

## Files to modify

- `cli/src/luks.rs:690-803` -- new `BackingPathResolver` trait, new
  `OwnershipError::BackingPathMismatch` and
  `OwnershipError::BackingPathResolveError`, extended
  `classify_mapper_ownership` signature and body. New
  `LuksError::MapperBackingMismatch` and
  `LuksError::MapperBackingResolveError` plus their
  `From<OwnershipError>` shim at `:714-729`.
- `cli/src/luks.rs:806-836,852-880` -- thread the resolver through
  `ensure_luks_open` / `ensure_luks_open_with_key_file`.
- `cli/src/probe.rs:16-21,121-208` -- `probe_mapper_open` gains a
  `&dyn BackingPathResolver` and `&ByIdPath`; `probe_config_disk`
  gains a resolver arg. New `ProbeError::MapperBackingMismatch` and
  `ProbeError::MapperBackingResolveError` plus extended
  `From<OwnershipError>`.
- All `probe_config_disk` callers -- `add.rs:1481`, `replace.rs:1171`,
  `recover.rs:2091,2178,2390,2471,2530,2936,3005`,
  `enroll_key_file.rs:87`, `mount.rs:231`, `status.rs:437`,
  `tui/probe.rs:224`. Each gains a `&dyn BackingPathResolver` arg (the
  command entry point constructs `RealBackingPathResolver` next to the
  `RealRunner`). TUI's `probe.rs` re-export and its
  `degrade-gracefully` `Err(_)` arm at `:236` survive without further
  change -- the new variant degrades like any other `ProbeError`.
- `cli/src/replace.rs:79-86` -- add two new variants:
  `ReplaceError::NewTargetMapperBackingMismatch { by_id, expected_path,
  found_path }` and `ReplaceError::NewTargetMapperBackingResolveError
  { by_id, resolved, source }`. Map `OwnershipError -> ReplaceError`
  via a local match at the execute-time call site (no blanket `From`
  needed; the variants are replace-specific).
- `cli/src/replace.rs:731-738` -- replace the comment-only skip with a
  fresh `classify_mapper_ownership` call. On UUID mismatch ->
  `NewTargetUuidMismatchAtOpen` (existing variant);  on backing-path
  mismatch -> `NewTargetMapperBackingMismatch` (new).
- `flake.nix` -- register the two new VM tests as `checks` entries
  (`replace-cloned-luks-header-rejected` and
  `braid-add-cloned-luks-header-rejected`) so `just test-vm` and
  `nix flake check` pick them up. Per `docs/testing.md:24`, new VM
  test files must have a matching flake check attribute or they do
  not run.
- `docs/decisions/024-luks-uuid-identity.md` -- refine the mapper
  ownership invariant. The current text frames LUKS UUID as the
  persistent identity used for membership correlation and live probe
  checks. Add a paragraph (likely in **Runtime Handles And Labels**,
  rule 5 or 7, or in **Limits And Non-Goals**) stating that mapper
  ownership at probe/open boundaries also requires the mapper's
  backing kernel block-device path to canonicalize to the configured
  by-id's path. LUKS UUID stays the persistent identity, but it is
  not sufficient at the live mapper boundary because cloned LUKS
  headers intentionally duplicate UUIDs. Reference
  `cli/src/luks.rs::classify_mapper_ownership` in the
  "Tests That Enforce This" section alongside the existing
  membership/lock entries.
- Test fixtures -- add `MockBackingPathResolver` once in
  `cli/src/test_fixtures/` (identity-by-default with a seedable
  overrides map). Re-export from each callers' `tests` module.

## Reused functions and patterns

- **`std::fs::canonicalize` wrapper pattern**: mirror
  `RealByIdResolver::canonicalize` at `cli/src/recover.rs:138-140` --
  `std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())`.
- **Error variant shape**: model `OwnershipError::BackingPathMismatch`
  on the existing `OwnershipError::Conflict` at `cli/src/luks.rs:702-707`;
  model `ReplaceError::NewTargetMapperBackingMismatch` on
  `ReplaceError::NewTargetUuidMismatchAtOpen` at `cli/src/replace.rs:79-86`.
- **Test runner helper**: extend `runner_with_active_mapper_uuid` at
  `cli/src/replace.rs:4984-5007` (or add a sibling
  `runner_with_active_mapper_backed_by`) for the unit tests' status +
  luksUUID seeding.
- **VM test prior art**: setup pattern from
  `tests/cli/replace-new-already-luks.py`; cloned-header fail-closed
  assertions from `tests/cli/braid-add-uuid-swap-rejected.py`;
  no-mutation pool.json + no-stranded-pending-op.json assertions from
  `tests/cli/replace-new-in-pool-guard.py:73-95`.
- **`is_partition_entry` helper**: not directly reused, but a reminder
  -- braid encrypts whole disks (no partitions), so the canonical
  comparison is `by_id` canonicalize -> whole-disk kernel path,
  matching the mapper backing's kernel path without partition-suffix
  arithmetic. Documented at `cli/src/recover.rs:13017-13032`.

## Implementation details

### `BackingPathResolver` trait

```rust
// cli/src/luks.rs (new)
pub(crate) trait BackingPathResolver {
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error>;
}

pub(crate) struct RealBackingPathResolver;

impl BackingPathResolver for RealBackingPathResolver {
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())
    }
}
```

### Extended `classify_mapper_ownership`

```rust
pub(crate) fn classify_mapper_ownership<R, F>(
    runner: &R,
    name: &str,
    mapper: &MapperName,
    expected_by_id: &ByIdPath,            // NEW
    resolver: &dyn BackingPathResolver,   // NEW
    expected_uuid: F,
) -> Result<MapperOwnership, OwnershipError>
where
    R: CommandRunner,
    F: FnOnce() -> Result<LuksUuid, OwnershipError>,
{
    // ... existing status + null-backing guards (luks.rs:758-777) ...
    let backing = status.device.as_deref()...; // existing extraction

    // NEW: canonicalize both sides and compare.
    let expected_path = resolver
        .canonicalize(expected_by_id.as_str())
        .map_err(|e| OwnershipError::BackingPathResolveError {
            name: name.to_owned(),
            by_id: expected_by_id.as_str().to_owned(),
            source: e,
        })?;
    let found_path = resolver
        .canonicalize(backing)
        .map_err(|e| OwnershipError::BackingPathResolveError {
            name: name.to_owned(),
            by_id: backing.to_owned(),
            source: e,
        })?;
    if expected_path != found_path {
        return Err(OwnershipError::BackingPathMismatch {
            name: name.to_owned(),
            expected_path,
            found_path,
        });
    }

    // ... existing UUID probe + compare (luks.rs:779-802) ...
}
```

The canonical-path check fires **before** the UUID probe so a
foreign-backed mapper with a coincidentally-matching UUID surfaces the
correct error (backing-path mismatch, with the correct remediation
text) rather than the misleading UUID-match-ish "Conflict".

### Open-boundary re-check at replace execute time

```rust
// cli/src/replace.rs:731-738 replacement
} else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
    // Open-boundary defense-in-depth for the mapper_open: true path:
    // re-classify ownership right before `pool_replace_device` so an
    // operator-staged cryptsetup close+reopen between planning and
    // execute cannot route pool data into a foreign disk via the
    // pre-existing mapper. Mirrors `:706-722` for the closed-mapper
    // arm. The tightened classifier (luks.rs) now checks both UUID
    // AND backing path, which closes the cloned-header hole.
    classify_mapper_ownership(
        runner,
        new_name.as_str(),
        &new_mn,
        &new_by_id,
        backing_path_resolver,
        || Ok(new_uuid.clone()),
    )
    .map_err(|e| match e {
        OwnershipError::BackingPathMismatch { expected_path, found_path, .. } =>
            ReplaceError::NewTargetMapperBackingMismatch {
                by_id: new_by_id.clone(),
                expected_path,
                found_path,
            },
        OwnershipError::BackingPathResolveError { by_id, source, .. } =>
            ReplaceError::NewTargetMapperBackingResolveError {
                by_id: new_by_id.clone(),
                resolved: by_id,
                source: source.to_string(),
            },
        OwnershipError::Conflict { found, .. } =>
            ReplaceError::NewTargetUuidMismatchAtOpen {
                by_id: new_by_id.clone(),
                expected: new_uuid.clone(),
                observed: found
                    .map(|u| u.as_str().to_owned())
                    .unwrap_or_else(|| "(no backing)".into()),
            },
        OwnershipError::Parse(e) => ReplaceError::Validation(e.to_string()),
        OwnershipError::Cmd(e) => ReplaceError::Validation(e.to_string()),
    })?;
}
```

### Error remediation wording

- `LuksError::MapperBackingMismatch` / `ProbeError::MapperBackingMismatch`
  -- "disk '{name}' mapper '/dev/mapper/braid-{name}' is open but
  backed by '{found_path}', not the configured disk at
  '{expected_path}'. Close the conflicting mapper with 'sudo
  cryptsetup close braid-{name}' and re-run."
- `LuksError::MapperBackingResolveError` /
  `ProbeError::MapperBackingResolveError` -- "disk '{name}' mapper
  backing-path check failed: could not canonicalize '{by_id}' ({source}).
  Check that the configured disk is plugged in and that udev has
  populated /dev/disk/by-id/."
- `ReplaceError::NewTargetMapperBackingMismatch` -- "replace target
  '{by_id}' open mapper backing mismatch: mapper is backed by
  '{found_path}', expected '{expected_path}' -- close the conflicting
  mapper with 'sudo cryptsetup close braid-<name>' and re-run."
- `ReplaceError::NewTargetMapperBackingResolveError` -- "replace
  target '{by_id}' open mapper backing-path check failed: could not
  canonicalize '{resolved}' ({source}) -- check that the disk is
  plugged in and that udev has populated /dev/disk/by-id/."

Per the project CLI-output-style rule, use `--` not em-dash.

## Tests

### Unit

- **`cli/src/luks.rs` tests** (sibling to `classify_mapper_ownership`
  conflict tests around `:1900-2150`):
  - Mapper active, UUID matches, but resolver reports differing
    canonical paths -> `OwnershipError::BackingPathMismatch`.
  - Twin happy-path test where canonical paths match.
  - Resolver `canonicalize(expected_by_id)` returns an `io::Error` ->
    `OwnershipError::BackingPathResolveError` with the source error
    preserved. Pinned distinctly from `BackingPathMismatch` so a
    future regression that collapses them shows up.
  - Resolver `canonicalize(backing)` returns an `io::Error` -> same
    `BackingPathResolveError` shape, naming the backing path.
- **`cli/src/replace.rs` tests** (sibling to seeds 630/631 at
  `:5094-5185`):
  - **Seed 632**: open-mapper open-boundary re-check fires
    `NewTargetMapperBackingMismatch` when the resolver reports drifted
    canonical paths, with no `BtrfsReplaceStart` issued and no
    `CryptsetupLuksOpen` issued.
  - **Seed 633**: control arm -- matching canonical paths, classifier
    returns `Owned`, execution proceeds.
- **`cli/src/probe.rs` tests**: extend `probe_config_disk_present_luks_open`
  at `:741-786` with a resolver fixture; add a twin test that returns
  `ProbeError::MapperBackingMismatch` when the resolver seeds a drifted
  target.
- **Resolve-error propagation at command boundaries** -- the
  classifier-layer test pins `OwnershipError::BackingPathResolveError`
  but does not exercise the conversions, so collapsing them back into
  generic validation or mismatch wording would not show up. Add
  focused unit tests at each conversion seam:
  - `cli/src/luks.rs` -- `ensure_luks_open` with a resolver that fails
    `canonicalize` on the by-id surfaces `LuksError::MapperBackingResolveError`
    (verified via `matches!` on the variant AND a substring assert on
    the rendered Display so the "check that the disk is plugged in"
    remediation text is pinned).
  - `cli/src/probe.rs` -- `probe_config_disk` with a resolver that
    fails `canonicalize` surfaces `ProbeError::MapperBackingResolveError`,
    with the same variant + Display assertions.
  - `cli/src/replace.rs` -- sibling to seeds 632/633, a new **Seed
    634** pins that the open-boundary `classify_mapper_ownership`
    call maps `OwnershipError::BackingPathResolveError` to
    `ReplaceError::NewTargetMapperBackingResolveError` (not to
    `Validation` and not to `NewTargetMapperBackingMismatch`), with
    no `BtrfsReplaceStart` issued.

### VM

- **`tests/cli/replace-cloned-luks-header-rejected.{nix,py}`** -- new.
  The cloned-header premise requires the **foreign mapper's backing
  to share the UUID of the configured `new_by_id` disk**, not of any
  pool member. Setup, in order:
  1. Build a healthy 2-disk pool from disk1, disk2.
  2. LUKS-format `/dev/disk/by-id/virtio-disk3` (the intended new
     target) so it carries a fresh UUID `U_new`.
  3. `cryptsetup luksHeaderBackup --header-backup-file /tmp/hdr
     /dev/disk/by-id/virtio-disk3`.
  4. `cryptsetup luksHeaderRestore --header-backup-file /tmp/hdr
     /dev/disk/by-id/virtio-disk4`. After this, disk4's LUKS header
     is byte-identical to disk3's.
  5. Assert `cryptsetup luksUUID /dev/disk/by-id/virtio-disk3 ==
     cryptsetup luksUUID /dev/disk/by-id/virtio-disk4` (both report
     `U_new`).
  6. Open the foreign disk under the target's mapper name:
     `cryptsetup open /dev/disk/by-id/virtio-disk4 braid-disk3` (with
     the test passphrase). Assert
     `cryptsetup status braid-disk3` reports `device: /dev/vd...` of
     **disk4**, not disk3.
  7. Run
     `braid replace --old disk2 --new disk3=/dev/disk/by-id/virtio-disk3`.
  Asserts: non-zero exit; output contains the new
  `MapperBackingMismatch` wording naming both canonical paths;
  `pool.json` bit-identical to baseline; `/var/lib/braid/pending-op.json`
  does not exist; `btrfs fi show` unchanged. A second subtest closes
  `braid-disk3` and re-runs the same `braid replace` to confirm the
  clean path (operator remediation honored) succeeds.
- **`tests/cli/braid-add-cloned-luks-header-rejected.{nix,py}`** -- new.
  Same cloned-header setup keyed off the `add` target's UUID, **but
  the setup must defeat the earlier add refusal points** so the
  failure provably comes from the classifier fix and not from a
  pre-existing gate:
  1. Build a 1-disk pool from disk1 so a mounted pool exists (needed
     by `validate_braid_preconditions` at `add.rs:138-152` -- it
     refuses any LUKS adoption without a mounted pool).
  2. LUKS-format `/dev/disk/by-id/virtio-disk3` **with `--label
     braid-disk3`** so `validate_braid_preconditions` at `add.rs:139`
     accepts the by-id (label must equal `braid-<name>`; otherwise
     the test exits via the label-mismatch arm and never reaches the
     classifier).
  3. `cryptsetup luksHeaderBackup` from disk3 -> `luksHeaderRestore`
     onto `/dev/disk/by-id/virtio-disk4`. Both raw devices now share
     `U_new` AND label `braid-disk3`.
  4. `cryptsetup open /dev/disk/by-id/virtio-disk4 braid-disk3` so
     the mapper is backed by the foreign disk. Assert via
     `cryptsetup status` that backing is disk4's kernel device.
  5. Run `braid add disk3=/dev/disk/by-id/virtio-disk3`.

  Asserts: non-zero exit; **output contains the exact
  `MapperBackingMismatch` wording naming both canonical paths**
  (this distinguishes the classifier refusal from the
  `classify_braid_disk_fsid` failure mode at `add.rs:1836-1839` --
  "no btrfs superblock" / "wrong FSID" -- which would also be
  non-zero but is the wrong refusal); `pool.json` bit-identical to
  baseline; `/var/lib/braid/pending-op.json` does not exist. A
  control subtest closes `braid-disk3` and re-runs `braid add` to
  confirm the operator-remediated path succeeds.
- **Existing tests stay green**: `tests/cli/replace-new-already-luks.py`
  (closed-mapper arm), `tests/cli/replace-2disk-pool.py`,
  `tests/cli/braid-add-uuid-swap-rejected.py`,
  `tests/cli/luks-mapper-drift.py`,
  `tests/cli/recover-replace-existing-luks-uuid-mismatch.py`.

## Out of scope (intentionally dropped)

- Rewriting `assert_new_uuid_unique` to scan all attached by-id paths
  -- the classifier fix at the gateway preempts the need.
- Touching `probe_observed_mapper_uuid` (`cli/src/probe_mapper_uuid.rs`)
  -- it is the post-commit close defense; cloned headers there only
  cause a "warning, mapper stays open" worst case, not data
  misrouting.
- The `recover.rs:2936` post-resize finish loop -- gated by
  `probe_config_disk` which now has the tightened check; downstream
  `ensure_keyfile_enrolled` operates against `by_id` directly.
- Migration / backward-compat for pending-op.json -- no schema change;
  replay re-runs `probe_config_disk` so the new gate applies uniformly
  to recovery.

## Verification

1. `just test-rust` -- unit tests pass (new seeds 632/633/634, new
   `BackingPathMismatch` and `BackingPathResolveError` arms at the
   classifier layer and at each conversion seam, all existing tests).
2. `nix flake check` resolves the two new attributes
   (`checks.aarch64-darwin.replace-cloned-luks-header-rejected` and
   `checks.aarch64-darwin.braid-add-cloned-luks-header-rejected`).
   Then `just test-vm replace-cloned-luks-header-rejected
   braid-add-cloned-luks-header-rejected` -- new VM tests pass.
3. `just test-vm replace-2disk-pool replace-new-already-luks
   braid-add-uuid-swap-rejected luks-mapper-drift
   recover-replace-existing-luks-uuid-mismatch
   replace-new-in-pool-guard` -- regression suite for the surrounding
   identity machinery.
4. `just test-parsers` -- ensure `parse_cryptsetup_status` still pins
   the backing-path field that the classifier now consumes
   end-to-end.
5. `just test-rust-unstable` -- golden parser tests against unstable
   fixtures (no fixture refresh expected; the change does not parse
   any new output).
