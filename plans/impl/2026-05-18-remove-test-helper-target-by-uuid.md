# Plan: Make remove render-test helper target by UUID

## Context

Production `remove` already follows the UUID identity model:

- `resolve_target_in_membership` maps the user name to a persisted
  `(LuksUuid, DiskName)`.
- `plan_remove` then locates the live `PoolDevice` with
  `pool.devices.iter().find(|d| d.luks_uuid == target_uuid)`.
- `RemoveWorkPlan` stores that `target_uuid` and the observed mapper
  from the matching live row.

The test-only helper `remove_present_work_plan_for_test` does not follow
that shape. It accepts a `MapperName`, finds the target by mapper, and
falls back to a placeholder UUID:

```rust
let target = pool
    .devices
    .iter()
    .find(|device| device.mapper == *mn)
    .cloned()
    .unwrap_or_else(|| PoolDevice {
        devid: 0,
        mapper: mn.clone(),
        luks_uuid: placeholder_uuid.clone(),
        underlying: String::new(),
    });
```

This helper is `#[cfg(test)]` and render-only, so it is not a production
bug. It is still misleading fixture code: it preserves a mapper-keyed
target selection pattern that production remove intentionally deleted.

## Intended outcome

The remove render-test helper selects its target by LUKS UUID, matching
the production planner's identity flow. It no longer creates placeholder
pool devices for absent mapper matches.

## Approach

All code changes stay in `cli/src/remove.rs`.

1. Change the helper signature to take the persisted `DiskName` plus
   the target `LuksUuid` -- mirroring how production
   `resolve_target_in_membership` returns the persisted
   `(LuksUuid, DiskName)` pair before `plan_remove` looks up the live
   `PoolDevice` by UUID:

   ```rust
   fn remove_present_work_plan_for_test(
       name: DiskName,
       target_uuid: &LuksUuid,
       pool: &PoolState,
       mount_point: &MountPoint,
   ) -> Result<RemoveWorkPlan, RemoveError>
   ```

   The `DiskName` represents the persisted membership name; the
   `LuksUuid` is the identity key used to find the live row.

2. Replace the mapper find plus placeholder fallback with a UUID lookup:

   ```rust
   let target = pool
       .devices
       .iter()
       .find(|device| device.luks_uuid == *target_uuid)
       .cloned()
       .expect("test pool must contain target UUID");
   ```

   A missing target is a broken test fixture, not a scenario this render
   helper needs to model.

3. Pass the supplied `name` straight into `RemoveWorkPlan::new`. Do not
   re-derive `DiskName` from the observed mapper -- a drifted
   `MapperName` is not guaranteed to round-trip through `DiskName::parse`,
   and re-deriving the name here would re-introduce the
   mapper-derived-identity dependency this cleanup is meant to remove.

4. Delete the placeholder UUID binding and fallback `PoolDevice`.

5. Update the two direct call sites:

   - `dry_run_render_3disk_removal`
   - `dry_run_render_2disk_removal_includes_balance`

   Each test already constructs a pool with the target disk's UUID; bind
   that UUID and the corresponding `DiskName` in the test and pass both
   to the helper.

## Regression tests

The existing render tests should continue to pass after their call sites
are updated.

Add one mandatory helper-level regression that proves the helper keys
target selection by UUID and uses the matched row's observed mapper for
the rendered commands:

- Build a `PoolState` whose `devices` list contains, in this order:
  1. a non-target decoy device with a distinct `LuksUuid` and its own
     unrelated `MapperName` (e.g. `braid-decoy`), and
  2. the target device, where the persisted `DiskName` (e.g. `disk1`)
     does not match the live `MapperName` (e.g. `braid-renamed`) -- a
     drift scenario.
- Call `remove_present_work_plan_for_test(disk1_name, &target_uuid,
  &pool, &mount_point)`.
- Assert against the rendered dry-run steps that the `btrfs device
  remove` step targets the target row's observed mapper path (i.e. uses
  `/dev/mapper/braid-renamed`, not `/dev/mapper/braid-decoy` and not
  `/dev/mapper/braid-disk1`), and that the `cryptsetup close` step
  closes that same observed mapper.

The decoy device guarantees that a mapper-keyed implementation could
not coincidentally pass, and the drifted observed mapper guarantees the
helper is reading the matched UUID row's live mapper rather than
synthesizing one from the persisted name.

## Verification

1. `just test-rust`

No VM test is necessary for this plan because the changed helper is
`#[cfg(test)]` only and production remove already has UUID-keyed target
resolution.

## Out of scope

- Do not change production `plan_remove`; it already resolves by
  membership UUID and then live pool UUID.
- Do not change `RemoveWorkPlan::new`; it already accepts the resolved
  target UUID plus the observed live `PoolDevice`.
