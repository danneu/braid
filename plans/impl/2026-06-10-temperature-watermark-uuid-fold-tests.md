# Plan: pin temperature-watermark UUID identity at the fold layer

## Context

`TemperatureReading.id` is keyed by `LuksUuid` (decision 024) specifically so
that TUI session temperature watermarks "survive device-path / name changes on
unplug/replug" (`cli/src/tui/model.rs:222-224`). The fold that realizes this is
`Message::PoolProbeFinished` in `cli/src/tui/app.rs:236-250`: it iterates
`pool.disk_temperature_readings.values()` and accumulates into
`model.session_temperature_stats` (a `HashMap<LuksUuid, TemperatureWatermark>`)
keyed by `reading.id`.

The four existing fold tests (`cli/src/tui/app.rs:847-955`) all reuse the disk
name `"toshiba"` **and** the same UUID across every probe, so none of them
exercise the identity axis the key choice exists for. Two distinct, operationally
meaningful behaviors are untested at every layer:

1. **Name change, UUID constant** (unplug/replug, mapper/name drift) -> one
   watermark must keep accumulating.
2. **Name constant, UUID changes** -- the display name is non-identity and
   reusable (decision 024 identity-boundary table: `DiskName` "Persistent
   identity? No"; the membership duplicate-name guard only rejects collisions
   among *current* members, `cli/src/membership.rs:351`). A name freed by
   `remove` can therefore be reassigned by `add` to a different physical disk,
   carrying a new LUKS UUID. The reused name must start a *separate* watermark,
   not inherit the prior disk's hi/lo range.

Case 2 is the sharper correctness payoff and is the more dangerous regression
(silently merging two physical disks' thermal history under a reused display
name), yet it is nowhere covered.

Note: this is **not** a `braid replace` story -- `plan_replace` rejects
`--old == --new` (`cli/src/replace.rs:1254`) and always assigns the new disk a
distinct name, so replace never reuses a display key. The reuse path is generic
membership churn, grounded in the ADR identity boundary above.

Note on scope: the *producer*-side contract -- that `reading.id` is populated
from the member's live LUKS UUID -- is already pinned by
`smartctl_health_for_present_member_uses_live_underlying`
(`cli/src/tui/probe.rs:1346`, asserts `reading.id == LuksUuid(1111...)`). This
plan deliberately covers the *consumer/fold* layer (`app.rs`), which is
complementary, not duplicative. The new tests' `// Why:` preambles will say so to
forestall a future "this duplicates probe.rs" review.

## Change 1 -- `cli/src/tui/app.rs`: add two fold tests

Add both tests to the existing `mod tests` block, next to the watermark tests
(after `probe_finished_missing_reading_preserves_watermark`, ~line 955). All
helpers are already in scope -- **no new imports**: `Model::new_demo`,
`sample_disk_names`, `PoolStatus::Loading`, `update`, `Message::PoolProbeFinished`,
`Duration`, `pool_probe_ok` (`app.rs:456`), and the local `pool_with_temperature`
(`app.rs:827`) / `temp_uuid` (`app.rs:823`) helpers. `sample_pool()`
(`cli/src/tui/demo.rs:233`) seeds `disk_temperature_readings` empty, so asserting
on `session_temperature_stats.len()` is sound.

### Test 1 -- watermark survives a disk-name change

```rust
// Intent: one disk's watermark must keep accumulating across a disk-name
//         change between probes -- two ticks, same LUKS UUID, different name
//         map key, produce a single watermark entry with sample_count == 2
//         and a widened range.
// Why: TemperatureReading.id is the LUKS UUID (decision 024) so session
//      watermarks survive device-path / name changes on unplug/replug. This
//      pins the fold/consumer side; the producer (id == live LUKS UUID) is
//      pinned by `cli/src/tui/probe.rs#smartctl_health_for_present_member_uses_live_underlying`.
//      A fold that folded on the name map key would fork one disk's history
//      into two entries and never cross the >=2 render threshold.
// Scenario: toshiba reports 38 C, then the same encrypted disk reappears
//           under a drifted name "toshiba-relabel" reporting 44 C.
#[test]
fn probe_finished_watermark_survives_disk_name_change() {
    let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
    let uuid = "11111111-1111-1111-1111-111111111111";
    let id = temp_uuid(uuid);
    update(
        &mut model,
        Message::PoolProbeFinished(
            pool_probe_ok(Some(pool_with_temperature("toshiba", uuid, 38))),
            Duration::from_millis(10),
        ),
    );
    update(
        &mut model,
        Message::PoolProbeFinished(
            pool_probe_ok(Some(pool_with_temperature("toshiba-relabel", uuid, 44))),
            Duration::from_millis(10),
        ),
    );
    assert_eq!(model.session_temperature_stats.len(), 1, "name change must not fork the entry");
    let w = model.session_temperature_stats.get(&id).unwrap();
    assert_eq!(w.min_celsius, 38);
    assert_eq!(w.max_celsius, 44);
    assert_eq!(w.sample_count, 2);
}
```

### Test 2 -- reused display name with a different UUID stays separate

```rust
// Intent: a probe that reuses a display name for a DIFFERENT LUKS UUID must
//         start a separate watermark, not merge the second disk's temperature
//         into the first disk's hi/lo range.
// Why: identity is the LUKS UUID, not the name -- decision 024's identity
//      table marks `DiskName` non-identity and reusable, and the membership
//      duplicate-name guard only rejects collisions among current members
//      (`cli/src/membership.rs#PoolMembership::insert`), so a name freed by
//      `remove` can be reassigned by `add` to a different physical disk
//      (fresh LUKS UUID). A name-keyed fold would silently contaminate one
//      disk's thermal history with another's. (Not a `braid replace`: replace
//      rejects --old == --new and always names the new disk distinctly,
//      `cli/src/replace.rs#plan_replace`.)
// Scenario: the bay shown as "bay3" reads 38 C; that disk is removed and a
//           different disk is later added under the freed name "bay3" (new
//           UUID), which reads 50 C in a later probe.
#[test]
fn probe_finished_watermark_separate_per_uuid_under_reused_name() {
    let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
    let first = "11111111-1111-1111-1111-111111111111";
    let second = "22222222-2222-2222-2222-222222222222";
    update(
        &mut model,
        Message::PoolProbeFinished(
            pool_probe_ok(Some(pool_with_temperature("bay3", first, 38))),
            Duration::from_millis(10),
        ),
    );
    update(
        &mut model,
        Message::PoolProbeFinished(
            pool_probe_ok(Some(pool_with_temperature("bay3", second, 50))),
            Duration::from_millis(10),
        ),
    );
    assert_eq!(model.session_temperature_stats.len(), 2, "reused name must not merge two UUIDs");
    let w_first = model.session_temperature_stats.get(&temp_uuid(first)).unwrap();
    assert_eq!((w_first.min_celsius, w_first.max_celsius, w_first.sample_count), (38, 38, 1));
    let w_second = model.session_temperature_stats.get(&temp_uuid(second)).unwrap();
    assert_eq!((w_second.min_celsius, w_second.max_celsius, w_second.sample_count), (50, 50, 1));
}
```

**Teeth (honest):** the `.get(&id: LuksUuid)` calls hold `session_temperature_stats`
to a UUID key at compile time; the `len()` assertions give a runtime guard that
survives even within UUID-keying -- Test 1 fails if a future change forks an
entry on a name-derived identity, Test 2 fails if the fold ever merges two UUIDs
under a reused name. Together they pin "the key is the UUID, and only the UUID."
There is deliberately no one-line "fold on the name" mutation to demonstrate
against: the map key type (`HashMap<LuksUuid, _>`) makes name-keying a
multi-site type change, not a local fold edit, so these tests act as the
compile anchor plus fork/merge guard rather than a single mutation target.

## Change 2 -- `docs/design/decisions/024-luks-uuid-identity.md`: ledger entry

The ADR keeps an explicit **Tests That Enforce This** list (lines 215-319) that
already cites `cli/src/tui/probe.rs` TUI tests but omits the temperature
watermark entirely. Add one entry near the other TUI/probe entries (~line 262):

```markdown
- `cli/src/tui/app.rs` unit tests pin that session temperature watermarks
  accumulate by LUKS UUID: a member's watermark keeps widening across a
  disk-name change between probes, and a reused display name carrying a
  different LUKS UUID (`DiskName` is non-identity and reusable) starts a
  separate watermark instead of merging two disks' thermal history.
  `cli/src/tui/probe.rs` pins the producer side -- the reading's `id` is the
  member's live LUKS UUID.
```

This keeps the change consistent with the project rule that behavior tied to an
invariant updates its decision doc (AGENTS.md "Architecture & authority").

## Files

- `cli/src/tui/app.rs` -- add two `#[test]` fns to the `mod tests` block.
- `docs/design/decisions/024-luks-uuid-identity.md` -- one ledger line.

No production code changes: the fold (`app.rs:236-250`) and the model are already
correct; this is pure regression coverage plus a doc-ledger touch.

## Verification

1. Run just the new tests:
   `cargo test --manifest-path cli/Cargo.toml probe_finished_watermark`
   -- both must pass. (The `probe_finished_watermark` substring matches only
   the two new fns; the existing `_seeds_temperature_watermark` / `_widens_*` /
   `_missing_reading_*` tests don't contain that contiguous substring.)
2. Full Rust suite: `just test-rust` -- green, no other test perturbed.
3. Docs build (link/ledger integrity): `just docs-build` -- no broken links.

No fold-mutation sanity step: as noted under Change 1, name-keying is a
multi-site type change rather than a local edit, so there is no faithful
one-line mutation to assert against -- the targeted test plus full suite are
the verification.

## Out of scope

- No change to the fold, model, view join, or `probe.rs` producer -- all correct.
- Not modifying the existing four watermark tests; they pin orthogonal seed /
  widen / missing-reading semantics and stay as-is.
