# Plan: resolve each unpooled member once in `build_disk_views`

## Context

`build_disk_views` (`cli/src/status.rs`) renders the unpooled-disk rows for
`braid status`. Its two unpooled loops re-run `membership.by_name(...)` -- the
documented O(n) linear scan (`membership.rs#by_name`) -- multiple times for the
*same* disk:

- The `config_disks` loop in `status.rs#build_disk_views`: up to **three** scans
  per disk -- the live-check, the mismatch classifier's recorded UUID
  (`PresentLuks` arm only), and the compact-row devid.
- The `probe_failures` loop in `status.rs#build_disk_views`: **two** scans per
  disk -- the live-check and the compact-row devid.

Each scan recovers a member that `build_status` already had in hand: its probe
loop iterates `membership.iter_by_name()`, maps the UUID away, calls
`probe_config_disk(... &member.name, &member.by_id ...)` (`status.rs#build_status`),
and that function stamps `ConfigDisk.name = member.name.clone()` on every return
path (`probe.rs#probe_config_disk`).
Because membership names are unique (insert Axis 2, `membership.rs#insert`),
`by_name(&cd.name)` provably returns the exact member that produced `cd`.

This is a Low-severity simplicity issue: pool cardinality is tiny so it is not a
perf problem. The cost is **readability and a latent coupling hazard** -- three
independent lookups obscure that the live-check, the classifier's recorded UUID,
and the devid all key off one member.

**Why a pivot, not the originally-proposed fix.** A review finding proposed
carrying the `(uuid, &DiskMember)` pair through `build_status` into a new
`build_disk_views` parameter shape. That removes the scan entirely but forces a
signature change plus rewrites across ~18 `build_disk_views(...)` test call sites,
for marginal benefit on a tiny pool. The simpler, equally-robust fix is to hoist
a single `by_name` resolution to the top of each loop and reuse it -- this
dissolves the redundancy and the coupling smell, aligns the unpooled loops with
the present-disk loop's existing "resolve member once" idiom (it binds
`matched_member = membership.by_uuid(...)` once and reuses it, in
`status.rs#build_disk_views`), and is a zero-signature-change edit that rewrites
none of the existing `build_disk_views` call sites (it adds one small
characterization test; see Tests).

Intended outcome: each unpooled member is resolved by name exactly once;
behavior and all output bytes are unchanged.

## Change

Confined entirely to the two unpooled loops in `build_disk_views`
(`cli/src/status.rs`). No signature change, no new types, no behavior change.

### `config_disks` loop

Hoist one resolution and reuse it at all three sites. `by_name` returns
`Option<(&LuksUuid, &DiskMember)>`, which is `Copy` (a tuple of two references),
so the single binding can be consumed by the live-check, the classifier, and the
devid filter without moves.

Before (3 scans):

```rust
for cd in config_disks {
    let membership_uuid_live = membership
        .by_name(&cd.name)
        .is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid));
    if membership_uuid_live {
        continue;
    }
    // ...
    ConfigDiskState::PresentLuks { uuid, .. } => {
        match luks::classify_member_luks_identity(
            uuid,
            membership.by_name(&cd.name).map(|(u, _)| u),
        ) { /* ... */ }
    }
    // ...
    CompactDrive {
        // ...
        devid: membership
            .by_name(&cd.name)
            .and_then(|(_, m)| m.devid)
            .filter(|d| alert_devids.contains(d)),
        status,
    },
}
```

After (1 scan):

```rust
for cd in config_disks {
    // Resolve the member once. It is provably the same DiskMember that
    // produced `cd` (probe_config_disk sets cd.name = member.name, and
    // membership names are unique), so the live-check, the mismatch
    // classifier's recorded UUID, and the compact-row devid all key off this
    // single lookup instead of re-scanning by name.
    let recorded = membership.by_name(&cd.name);
    let membership_uuid_live =
        recorded.is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid));
    if membership_uuid_live {
        continue;
    }
    // ...
    ConfigDiskState::PresentLuks { uuid, .. } => {
        match luks::classify_member_luks_identity(uuid, recorded.map(|(u, _)| u)) {
            /* ... */
        }
    }
    // ...
    CompactDrive {
        // ...
        devid: recorded
            .and_then(|(_, m)| m.devid)
            .filter(|d| alert_devids.contains(d)),
        status,
    },
}
```

### `probe_failures` loop (the sibling instance)

The same root-cause pattern, two scans -> one. The finding cited only
`config_disks`, but this loop carries the identical redundancy and is fixed the
same way so the two loops stay consistent.

```rust
for failure in probe_failures {
    let recorded = membership.by_name(&failure.name);
    let membership_uuid_live =
        recorded.is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid));
    if membership_uuid_live {
        continue;
    }
    // ...
    CompactDrive {
        // ...
        devid: recorded
            .and_then(|(_, m)| m.devid)
            .filter(|d| alert_devids.contains(d)),
        status: DiskStatus::Unknown,
    },
}
```

### Explicitly out of scope

- **No signature change** to `build_disk_views`, and `membership` stays a
  parameter -- the present-disk loop still needs it via `by_uuid`.
- **No carrying the pair through `build_status`** (the heavier proposed fix).
- **No merging the two loops.** They diverge meaningfully (`config_disks`
  classifies `ConfigDiskState` into a status + LUKS UUID; `probe_failures` always
  emits `Unknown`). Unifying them would trade clarity for a smaller line count --
  over-reach for this change.
- **No change to `classify_member_luks_identity`.** Its `Option<&LuksUuid>`
  signature and `Unrecorded` arm stay -- the TUI passes a possibly-`None`
  recorded UUID (`tui/probe.rs#probe_pool_for_tui`). Status simply feeds it
  `recorded.map(...)` from the hoisted binding, exactly as before.
- **No touching `by_name` call sites elsewhere** (`add`/`remove`/`replace`/
  `recover`/`unlock`/`discover`). Those legitimately resolve an operator-supplied
  name string to a member the caller does not already hold -- a different pattern.

## Files

- `cli/src/status.rs` -- the only file modified; the two unpooled loops inside
  `build_disk_views`.

## Tests

The **`config_disks` 3->1 hoist** is already fully pinned by structure-insensitive
tests in `cli/src/status.rs` that assert on output, never on lookup counts -- a
mis-wired site fails one of these:

- live-check skip -> `build_disk_views_skips_unpooled_row_when_membership_uuid_live_for_present_not_luks`
- classifier `Mismatch` -> `build_disk_views_foreign_config_uuid_classified_as_uuid_mismatch`
- classifier `Matches` -> `build_disk_views_present_luks_matching_uuid_offline_classified_as_offline`
- compact devid shown vs hidden -> `build_disk_views_missing_member_shows_devid_when_live_confirmed`
  and `build_disk_views_missing_member_hides_stale_persisted_devid`

The **`probe_failures` 2->1 hoist** is only partially pinned today: the sole test
that builds `ConfigProbeFailure` rows (`status_unpooled_rows_sorted_by_name_across_ok_and_failures`)
passes `PoolMembership::empty()` + `status_pool_empty()`, so its
`by_name(&failure.name)` is always `None` -- the live-check skip's *true* outcome
and the devid-*shown* outcome are never exercised. Add one test to close the
behaviorally important gap:

- **New test** `build_disk_views_skips_probe_failure_row_when_membership_uuid_live`.
  Mirror the pool+membership setup of the `config_disks` skip test
  (`build_disk_views_skips_unpooled_row_when_membership_uuid_live_for_present_not_luks`):
  membership `disk1` at UUID U1, the live pool reports U1 present (`missing_count`
  0), but feed `disk1` as a `ConfigProbeFailure` with `config_disks == &[]`.
  Assert exactly one row -- `DiskStatus::Present`, named `disk1` -- i.e. NO second
  `Unknown` unpooled row. This pins the decision-024 tolerated-drift skip (a
  member live-and-healthy under another mapper but errored in probe must not
  double-render), whose `true` branch the hoist touches. It is a characterization
  test: green both before and after the refactor.

The `probe_failures` devid-*shown* branch is deliberately left to lean on its
byte-identical `config_disks` twin (`build_disk_views_missing_member_shows_devid_when_live_confirmed`)
-- a dedicated test there would only re-assert mechanically identical code, so it
is not worth the test mass.

No test counts `by_name` calls or spies on membership lookups, so the call-count
reduction itself breaks nothing. Existing fixtures (`status_membership_1disk`,
`status_cfg_absent`, ... in `cli/src/test_fixtures/status.rs`) supply the inputs
the new test needs and require no changes.

## Docs

None. Decision 024 (`docs/design/decisions/024-luks-uuid-identity.md`) and the
nearby code comments document observable behavior (UUID-keyed identity,
swap/reformat detection, the display-join rule for devids) -- not the lookup
mechanism. A pure readability refactor with byte-identical output does not
contradict them.

## Verification

1. `just test-rust` -- the `build_disk_views_*` and `status_*` tests above must
   stay green, confirming behavior is unchanged.
2. `just clippy` -- confirms the hoisted `Copy` binding triggers no borrow/lint
   regressions.
3. Spot-read the diff: every `membership.by_name(&cd.name)` /
   `membership.by_name(&failure.name)` inside the two loops is replaced by a use
   of the single per-iteration `recorded` binding; exactly one `by_name` call
   remains per loop iteration.
