# Plan: Surface mapper conflicts in the TUI disk table

## Context

`cli/src/tui/probe.rs:238` ends the unpooled-disk classifier loop with
a catch-all `Err(_) => continue, // degrade gracefully`. That arm
silently swallows every "mapper is in a bad state" signal that
`probe_config_disk` can raise:

- `ProbeError::MapperBackingMismatch` -- mapper `braid-<name>` is open
  but its backing block device canonicalizes to a different path than
  the configured by-id disk. This is the main "hijack" shape today:
  an unrelated LUKS device opened at the mapper name. Source path:
  `cli/src/luks.rs:863-869` (path-comparison fires before any UUID
  comparison).
- `ProbeError::MapperConflict` -- mapper is open against the **same**
  backing path but with a null backing (stale dm-crypt) or a
  different LUKS UUID. Source paths: `cli/src/luks.rs:837-844`
  (null-backing) and `cli/src/luks.rs:877-895` (non-LUKS or
  UUID-mismatch on the same canonical device).
- `ProbeError::MapperBackingResolveError` -- canonicalize on the
  by-id or backing path failed (e.g. dangling symlink, EACCES).
  Source path: `cli/src/luks.rs:849-862`.

When the catch-all fires, `unpooled_disks` has no entry for the disk
and the view's fallback at `cli/src/tui/view/mod.rs:789-793` renders
it as the literal yellow `"missing"` -- identical to an unplugged
cable.

Three problems with this:

1. **Hidden diagnostic.** Decision 024
   (`docs/decisions/024-luks-uuid-identity.md`) explicitly lists "earlier
   clone and swap detection" and "cleanup follows observed ownership"
   as benefits of the UUID-keyed identity model. The TUI is the
   primary read-only diagnostic surface; a hijack-shaped failure is
   exactly the kind of state operators come to the TUI to see.
2. **Inconsistent with siblings.** `cli/src/monitor.rs:62-69` and
   `cli/src/lock.rs:729-740` already match `ProbeError` exhaustively
   with explicit `MapperConflict { .. }` / `MapperBackingMismatch { ..
   }` arms. `cli/src/status.rs:437` propagates them as a hard
   `StatusError::Probe(..)` with a precise "close braid-<name> and
   re-run" message. The TUI is the only diagnostic surface that hides
   the case.
3. **Pattern already exists for refining "degraded gracefully" cases.**
   The same loop already surfaces `ProbeError::UnsupportedLuksVersion`
   as `UnpooledDiskRender::WrongLuksVersion(version)` instead of
   collapsing into "missing". The mapper-ownership variants should
   follow the same shape.

Goal: render a hijacked-mapper disk (either path-mismatch or
UUID-mismatch) as a distinct, red `"mapper conflict"` cell instead of
yellow `"missing"`, and tighten the catch-all into an exhaustive
match so future `ProbeError` variants cannot be silently lost the
same way.

## Approach

Pattern-follow the existing `UnsupportedLuksVersion` →
`UnpooledDiskRender::WrongLuksVersion` plumbing. Concretely:

1. New unit enum variant `UnpooledDiskRender::MapperHijacked`. The
   variant carries no payload: both `MapperConflict` (which holds an
   `Option<LuksUuid>`) and `MapperBackingMismatch` (which holds two
   path strings) collapse into the same render state, and the
   per-shape detail is already in each error's `Display` impl for the
   eventual unlock-time message. A unit variant keeps
   `UnpooledDiskRender` `Copy`-able and avoids a sub-enum that no
   consumer needs.
2. Replace the `Err(_) => continue` catch-all in
   `probe_pool_for_tui` (`cli/src/tui/probe.rs:226-238`) with two
   explicit "hijack" arms (`MapperConflict { .. }` and
   `MapperBackingMismatch { .. }`) that both insert the new variant,
   plus an exhaustive residual arm naming every remaining
   `ProbeError` variant -- including `MapperBackingResolveError` --
   so the compiler enforces future-variant gating, mirroring
   `monitor.rs:62-69` and `lock.rs:729-740`.
3. Render the new variant in `unpooled_disk_status_cell` as the
   lowercase `"mapper conflict"` label in `Color::Red`, alongside the
   other serious config-level issues (`LuksHeaderDamaged`,
   `LuksHeaderUnreadable`, `WrongLuksVersion`).
4. Pin behavior with two new `tui::probe` tests -- the **primary**
   one for `MapperBackingMismatch` (the common
   unrelated-device-opened-as-`braid-<name>` shape) and a secondary
   for `MapperConflict { found: None }` (stale dm-crypt with null
   backing) -- and extend the existing view-layer variant-rendering
   test with the new variant and a `Color::Red` assertion.

## Critical files

- `cli/src/tui/model.rs` -- `UnpooledDiskRender` enum at lines 166-185.
- `cli/src/tui/probe.rs` -- unpooled classifier loop at lines 218-267
  and test module starting around line 658.
- `cli/src/tui/view/mod.rs` -- `unpooled_disk_status_cell` at lines
  716-733 and the variant-rendering test at lines 2010-2039.

Reference files (read-only; existing patterns to follow):

- `cli/src/probe.rs:85-110` -- `ProbeError::MapperConflict`,
  `MapperBackingMismatch`, and `MapperBackingResolveError` variant
  shapes plus their `From<OwnershipError>` mapping at lines 122-156.
- `cli/src/luks.rs:700-715` -- `BackingPathResolver` trait and the
  production / mock impls.
- `cli/src/luks.rs:816-896` -- `classify_mapper_ownership` order of
  evaluation. Hijack triage is path-first: `BackingPathMismatch`
  fires before any UUID comparison, so unrelated-device hijack lands
  in `MapperBackingMismatch`, not `MapperConflict`.
- `cli/src/monitor.rs:46-69` -- "exhaustive over every ProbeError
  variant on purpose" comment plus the exhaustive arm pattern to copy.
- `cli/src/lock.rs:729-740` -- second instance of the same pattern.
- `cli/src/test_fixtures/shared.rs:194-249` --
  `MockBackingPathResolver::with_path` / `::default()` and the
  `mock_virtio_backing_path_resolver()` helper already used by the
  other `tui::probe` tests. Unknown paths return themselves, so a
  hijack-path-mismatch fixture only needs the legitimate by-id and
  the cryptsetup-reported backing to be different strings (no
  override seeding required).

## Implementation steps

### 1. Extend the render enum (`cli/src/tui/model.rs:166`)

- Keep the existing `#[derive(Clone, Copy, Debug, PartialEq, Eq)]`.
  The new variant is unit (no payload) so `Copy` still applies.
- Add a new variant:

  ```rust
  /// `probe_config_disk` returned `ProbeError::MapperConflict` or
  /// `ProbeError::MapperBackingMismatch`. The expected mapper
  /// `braid-<DiskName>` is open against the wrong backing -- either
  /// a different physical block device (path mismatch, the common
  /// "unrelated device opened as braid-<name>" case), a stale
  /// dm-crypt with no backing, or the same physical device with a
  /// different LUKS UUID (reformatted or re-UUIDed in place; LUKS
  /// keyslot rotations preserve the UUID). Recovery for all shapes
  /// is `sudo cryptsetup close braid-<name>` then re-unlock, which
  /// is why one render state covers both errors. Detailed
  /// expected/found data lives in each underlying `ProbeError`'s
  /// `Display` impl and surfaces in the unlock-time error message.
  MapperHijacked,
  ```

### 2. Tighten the classifier loop (`cli/src/tui/probe.rs:226-238`)

Replace:

```rust
Err(ProbeError::UnsupportedLuksVersion { version, .. }) => { ... }
Err(_) => continue, // degrade gracefully -- skip this disk
```

with two hijack arms plus an exhaustive residual arm:

```rust
Err(ProbeError::UnsupportedLuksVersion { version, .. }) => { ... }
Err(ProbeError::MapperBackingMismatch { .. })
    | Err(ProbeError::MapperConflict { .. }) => {
    // Surface mapper hijack / drift / stale dm-crypt explicitly so
    // the operator sees a distinct red "mapper conflict" cell
    // instead of the yellow "missing" rendering used for unplugged
    // disks. Both variants share one render state because the
    // operator's recovery is identical -- close the offending
    // mapper, then re-unlock. classify_mapper_ownership checks the
    // backing path before UUID, so the common "unrelated device
    // opened as braid-<name>" case arrives here as
    // MapperBackingMismatch (cli/src/luks.rs:863-869), while
    // null-backing and same-path-different-UUID arrive as
    // MapperConflict. See cli/src/luks.rs:816-896.
    unpooled_by_name
        .insert(disk_name.clone(), UnpooledDiskRender::MapperHijacked);
    continue;
}
// Exhaustive residual arm: future ProbeError variants must come
// here and be classified, not silently swallowed. NotBtrfs /
// PoolDevice / MountInfo are statically unreachable from
// probe_config_disk (they come from probe_pool / mountinfo
// parsing) but listed for the future-variant compile gate, the
// same way monitor.rs:62-69 and lock.rs:729-740 list them. Cmd,
// Parse, and MapperBackingResolveError continue silently because
// tool / udev failure is not per-disk-meaningful -- a missing
// cryptsetup binary or a stale by-id symlink would otherwise
// surface as one "probe failed" cell per declared disk, drowning
// the actual signal. braid doctor / monitor cover that surface.
Err(ProbeError::Cmd(_)
    | ProbeError::Parse(_)
    | ProbeError::PoolDevice { .. }
    | ProbeError::NotBtrfs { .. }
    | ProbeError::MapperBackingResolveError { .. }
    | ProbeError::MountInfo(_)) => continue,
```

### 3. Render the new variant (`cli/src/tui/view/mod.rs:716-733`)

Add a new arm to `unpooled_disk_status_cell`:

```rust
UnpooledDiskRender::MapperHijacked => {
    Span::styled("mapper conflict", Style::default().fg(Color::Red))
}
```

Naming and color rationale:

- Lowercase `"mapper conflict"` matches the existing label casing
  (`"missing"`, `"unknown"`) and re-uses the same noun as
  `ProbeError::MapperConflict` / `MapperBackingMismatch`'s
  user-facing error templates (`cli/src/probe.rs:78-100` -- both
  produce "Close the conflicting mapper with 'sudo cryptsetup close
  braid-{name}' and re-run"). The operator sees the same vocabulary
  in the TUI cell and in the eventual unlock-time error message.
- `Color::Red` matches the severity tier of
  `LuksHeaderDamaged` / `LuksHeaderUnreadable` / `WrongLuksVersion` --
  serious config-level issues that need operator intervention but do
  not indicate data loss. Yellow would put it alongside
  `Missing`/`UnknownLuks` (unplugged / unrelated), which is the
  current wrong behavior we are fixing.

The variant intentionally carries no payload. The two underlying
errors have different fields (`Option<LuksUuid>` vs two path strings)
and one cell label covers both. Rich expected/found detail already
lives in each `ProbeError` variant's `Display` impl and surfaces in
the unlock-time message; if a future disk-detail popup wants to
render it, the resolver and `Model::disk_luks_uuid` are accessible
at render time.

### 4. Pin the classifier behavior (`cli/src/tui/probe.rs` test module)

Add two tests modeled after
`unpooled_disk_wrong_luks_version_classified_correctly`
(`cli/src/tui/probe.rs:1660-1705`). Reuse the existing helpers:
`one_disk_mounted_pool_runner`, `tui_disk_luks_uuid`,
`tui_disk_devid`, `test_paths`, `StubFs::with_paths`, `ok_raw`, and
`crate::test_fixtures::mock_virtio_backing_path_resolver()` (the same
resolver every other tui-probe test passes; unknown paths --
including `/dev/disk/by-id/braid-ironwolf` and any `cryptsetup
status`-reported backing path -- pass through unchanged, which is
exactly what the hijack test needs).

**Test A (primary): path-mismatch hijack →
`ProbeError::MapperBackingMismatch`**

This is the common shape: an unrelated LUKS device is opened at the
expected mapper name. `classify_mapper_ownership` fires
`BackingPathMismatch` before any UUID comparison
(`cli/src/luks.rs:863-869`).

Stack mock entries so:

- `CryptsetupLuksUuid` on `/dev/disk/by-id/braid-ironwolf` returns
  ironwolf's real UUID (e.g. `22222222-...`).
- `CryptsetupLuksDumpText` on the same path reports LUKS2.
- `CryptsetupStatus` for mapper `braid-ironwolf` reports an active
  mapper backed by a path that differs from
  `/dev/disk/by-id/braid-ironwolf` (e.g. `/dev/vdz`). With the
  shared mock resolver, both paths canonicalize to themselves, so
  the strings differ → `MapperBackingMismatch` → silently triggers
  the new arm before any `CryptsetupLuksUuid` call on the backing
  path.

Assert `pool.unpooled_disks.get("ironwolf") ==
Some(&UnpooledDiskRender::MapperHijacked)`.

**Test B (secondary): null-backing stale dm-crypt →
`ProbeError::MapperConflict { found: None }`**

Stack mock entries so:

- `CryptsetupLuksUuid` on the by-id path returns ironwolf's real UUID.
- `CryptsetupLuksDumpText` reports LUKS2.
- `CryptsetupStatus` for `braid-ironwolf` reports active with the
  `device:` field empty or `(null)` (matching `cryptsetup status`'s
  output for a stale dm-crypt -- see `cli/src/luks.rs:837-844`).

Assert `pool.unpooled_disks.get("ironwolf") ==
Some(&UnpooledDiskRender::MapperHijacked)`.

The pair mirrors the two-shape coverage of `probe_mapper_open` at
`cli/src/probe.rs:920-946` (BackingPathMismatch) and
`cli/src/probe.rs:1057-1082` (Conflict with null backing). A third
test for the `MapperConflict { found: Some }` shape
(same-canonical-path, different LUKS UUID; e.g. reformatted or
re-UUIDed in place) is
deliberately out of scope -- it is a contrived TOCTOU shape that
would require seeding the resolver to collapse two different
cryptsetup-reported paths to the same canonical path, and it
contributes no additional classifier-arm coverage beyond Test B.

### 5. Extend the view-layer test (`cli/src/tui/view/mod.rs:2010-2039`)

Add one entry to `unpooled_disk_status_cell_renders_each_variant`:

```rust
("foxtrot".to_owned(), UnpooledDiskRender::MapperHijacked),
```

Pin both the label content **and** the `Color::Red` severity tier --
the color is a load-bearing requirement (it places the cell in the
serious-config-issue tier alongside `LuksHeaderDamaged`,
`LuksHeaderUnreadable`, and `WrongLuksVersion`, not in the
unplugged/unrelated tier with `Missing` and `UnknownLuks`).
Content-only assertions would let an implementation regression to
yellow or default styling pass silently.

Concretely, the existing closure at `view/mod.rs:2023-2028` only
extracts `.content`. For the new entry, add a direct span-level
assertion that checks both fields:

```rust
assert_eq!(cell("foxtrot"), "mapper conflict");

let foxtrot_span = unpooled_disk_status_cell(&pool, "foxtrot")
    .expect("expected an entry");
assert_eq!(foxtrot_span.style.fg, Some(Color::Red));
```

Update the existing "foxtrot" no-entry assertion
(`view/mod.rs:2036`) to use a different name (e.g. `"hotel"`) since
`foxtrot` is now used. The test is a runtime assertion test, not
exhaustiveness-checked by the compiler -- but the view's `match` at
`view/mod.rs:717` has no wildcard, so adding the variant will
compile-error there if the render arm is missed.

## Out of scope

- No change to `ProbeError` variants (`MapperConflict`,
  `MapperBackingMismatch`, `MapperBackingResolveError`) or to
  `probe_config_disk` / `classify_mapper_ownership` -- the gateway
  already returns the right shapes; only the TUI consumer changes.
- No change to `pool.json`, journals, or any mutating command.
- No change to `braid status`'s hard-error propagation -- the TUI
  refinement does not affect the CLI surface.
- No new VM test: `tui` rendering is covered by Rust unit tests
  exclusively, mirroring how `WrongLuksVersion` is pinned today.
- No widening of `ConfigDiskState`: the new render variant is
  diagnostic-only and lives at the TUI boundary, the same quarantine
  pattern used by `LuksHeaderUnreadable`/`Damaged` (see commit
  15186f0's "the probe-layer `ConfigDiskState` enum stays coarse on
  purpose" note).

## Verification

Run from the repo root.

1. **Compiler exhaustiveness gate.** The exhaustive match in step 2
   and the no-wildcard match in `unpooled_disk_status_cell` are the
   compile-time gates. If either is incomplete, `cargo build` fails.

2. **Rust unit tests.**

   ```
   just test-rust
   ```

   Expect both new probe tests
   (`unpooled_disk_mapper_backing_mismatch_classified_correctly` for
   the primary path-mismatch hijack and
   `unpooled_disk_mapper_conflict_null_backing_classified_correctly`
   for the stale-dm-crypt case, or similar names) and the extended
   view-layer variant test to pass.

3. **Existing test suite is not regressed.** The same `just test-rust`
   run covers the existing
   `unpooled_disk_absent_classified_as_missing`,
   `unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks`,
   `unpooled_disk_present_not_luks_unreadable_classified_correctly`,
   `unpooled_disk_present_not_luks_damaged_classified_correctly`,
   `unpooled_disk_wrong_luks_version_classified_correctly`, and
   `unpooled_disk_status_cell_renders_each_variant`. None should
   change behavior.

4. **VM tests.** No new VM test needed; run `just test-vm` if any
   sanity check is desired but no failure is expected -- this change
   touches only Rust-level TUI rendering, not any CLI command surface
   or systemd unit.

5. **Manual TUI check (optional, only if a VM is already up).**
   The Rust unit tests are the load-bearing verification. A manual
   repro is non-trivial because the bug only surfaces for a declared
   disk that is **absent from live `btrfs device usage`** (i.e. no
   devid row at all -- not present, not btrfs-MISSING, not
   `null_underlying`). `probe_pool_for_tui`
   (`cli/src/tui/probe.rs:54-82, 84-97`) explicitly binds
   btrfs-MISSING and `null_underlying` devices back into
   `disk_usage` via the persisted-devid map, so those states do not
   reach the unpooled classifier. The `tests/cli/luks-mapper-drift.py`
   scenario (a drifted but member-owned mapper that *is* mounted)
   also does not trigger this path, since the live LUKS UUID still
   joins back to the member name and populates `disk_usage`. The
   required topology mirrors the unit tests: a mounted pool whose
   live `btrfs device usage` has **no row** for one of the declared
   members (e.g. a member that was `btrfs device remove`d but left in
   `pool.json`, or a partial-add state), that member's by-id path is
   reachable, and `/dev/mapper/braid-<that-member-name>` is
   externally `cryptsetup open`ed against an unrelated LUKS device.
   The TUI's disk-table row for that member should render a red
   `"mapper conflict"` cell instead of the previous yellow
   `"missing"`.
