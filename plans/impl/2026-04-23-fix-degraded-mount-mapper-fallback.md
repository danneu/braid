# Fix: degraded mount picks stale mapper in plan_open_pool

## Context

`cli/src/mount.rs:246-250` picks `mount_device` via a two-step fallback:

```rust
let mount_key = to_unlock
    .first()
    .map(|(k, _)| k.as_str())
    .or_else(|| membership.disks.keys().next().map(|k| k.as_str()))
    .unwrap_or("unknown");
```

The second leg — `membership.disks.keys().next()` — iterates the `BTreeMap` in
sort order with no state filter. When:

1. `--allow-degraded` is in effect, AND
2. the alphabetically-first member (e.g. `disk1`) is `Absent` or
   `PresentNotLuks`, AND
3. at least one other member is `PresentLuks` with `mapper_open=true`,

then `to_unlock` is empty, `any_open=true`, and the fallback picks the bad
first-key. `mount_device` becomes `/dev/mapper/braid-disk1`, which does not
exist. The subsequent `mount` call fails with a confusing "no such device"
error even though a valid mapper (e.g. `braid-disk2`) is already open and
btrfs would happily mount the pool from any member in the RAID1 set.

The unit-test suite does not cover this combination: `mount_skip_already_open`
has both disks present+luks+open (disk1 is a valid pick), and
`mount_degraded_with_flag` has disk1 present+luks+closed (so `to_unlock`
picks it up). Neither exercises "first disk absent + second disk's mapper
open", which is the real production scenario on a degraded unlock.

Callers (`cmd_unlock` in `cli/src/unlock.rs`, `cmd_recover` in
`cli/src/recover.rs`) do not recompute `mount_device`; they use the plan's
field verbatim. Fixing `plan_open_pool` fixes every code path.

## Fix

Record the first already-open mapper's disk name during the probe loop, and
use that as the fallback instead of `membership.disks.keys().next()`. The
variable encodes the real invariant: when there is no unlock work left, the
fallback must be a mapper that already exists.

### Change `cli/src/mount.rs` (one file)

1. Add a local inside `plan_open_pool` before the probe loop:
   ```rust
   let mut first_open_mapper: Option<String> = None;
   ```
2. In the `ConfigDiskState::PresentLuks { uuid, mapper_open }` arm (after
   the UUID mismatch check), record the name on the `*mapper_open` branch:
   ```rust
   if *mapper_open {
       if first_open_mapper.is_none() {
           first_open_mapper = Some(name.clone());
       }
       eprintln!("{}  disk: {:<10}already open", tag("ok"), name);
       any_open = true;
   } else {
       // existing to_unlock branch unchanged
   }
   ```
3. Replace the fallback at lines 246-250 with:
   ```rust
   let mount_key = to_unlock
       .first()
       .map(|(k, _)| k.as_str())
       .or(first_open_mapper.as_deref())
       .unwrap_or("unknown");
   ```

The `"unknown"` sentinel is now unreachable in practice (the empty-both
guard at line 231 already errors when `to_unlock.is_empty() && !any_open`,
and `any_open == true` implies exactly one path: at least one
`mapper_open=true` was seen, therefore `first_open_mapper` is `Some`).
Keep it as a defensive default rather than changing the shape of the
expression -- a future refactor of the probe loop should not silently
reintroduce a stale-mapper panic.

## Test (same file, failure-layer unit tests)

The regression scenario must make `to_unlock` stay empty so the broken
fallback is actually exercised. A disk in `PresentLuks` with
`mapper_open=false` would be pushed into `to_unlock` and make
`to_unlock.first()` win regardless of the fix -- neither catching the bug
nor exercising the fallback. The scenario therefore needs every surviving
member to already have `mapper_open=true`.

Add two tests to the `tests` module in `cli/src/mount.rs`, reusing existing
helpers (`three_disk_membership`, `luks_uuid_ok`, the `MockRunner` builder
pattern from `base_two_disk_runner`, `with_luks_dump_text_luks2`):

### `plan_open_pool_degraded_first_absent_picks_open_mapper` (primary regression)

Unit-level, calls `plan_open_pool` directly and asserts on
`plan.mount_device`. This is the test that must fail when the fix is
reverted.

- 3-disk membership (disk1, disk2, disk3).
- `MockFs`: `/dev/disk/by-id/virtio-disk2`, `/dev/disk/by-id/virtio-disk3`,
  `/dev/mapper/braid-disk2`, `/dev/mapper/braid-disk3`. disk1 absent; disk2
  and disk3 both `PresentLuks` with mappers already open.
- Seed `luksUUID` + `luksDump` for disk2 and disk3. MountpointCheck returns
  non-zero (pool not yet mounted).
- `allow_degraded = true`, `command_hint = "unlock"`.
- Assert: `plan.to_unlock.is_empty()`, `plan.any_open == true`,
  `plan.any_missing_member == true`, and
  `plan.mount_device == "/dev/mapper/braid-disk2"`.
- Pre-fix, `mount_device` resolves to `/dev/mapper/braid-disk1` (the absent
  disk) via `membership.disks.keys().next()`. Post-fix, it resolves to
  `/dev/mapper/braid-disk2` (the first open mapper in BTreeMap order).

### `mount_degraded_first_absent_all_open_uses_open_mapper` (end-to-end wiring)

Proves the plan's `mount_device` is the value that actually gets passed to
`CmdRequest::MountWithOptions`, catching any future refactor that might
recompute `mount_device` downstream.

- Same setup as above (disk1 absent, disk2+disk3 open).
- Seed `BtrfsDeviceScanAll` and `MountWithOptions { device:
  "/dev/mapper/braid-disk2", options: ["degraded"] }`.
- No passphrase mocks (nothing to unlock).
- Call `open_and_mount_for_test(..., None, true, "unlock")`. Assert
  `Ok(true)`. The `MockRunner` is strict about seeded requests, so any
  attempt to mount `/dev/mapper/braid-disk1` surfaces as a missing-mock
  error and fails the test -- that is the regression signal.

## Verification

1. `just test-rust` -- confirms the two new unit tests pass and no existing
   `mount::tests::*` regress. The primary regression test
   (`plan_open_pool_degraded_first_absent_picks_open_mapper`) must fail if
   `or(first_open_mapper.as_deref())` is reverted to
   `or_else(|| membership.disks.keys().next()...)`.
2. Spot-check `cli/src/unlock.rs` and `cli/src/recover.rs` consumers — no
   code changes expected; they read `plan.mount_device` only.
3. No VM test needed. The bug is pure in-process planning logic with no
   kernel/LUKS state interaction; the unit tests cover it at the failure
   layer.

## Critical Files

- `cli/src/mount.rs` — `plan_open_pool` (lines 153-259) and its `tests`
  module. Single file touched.

## Out of Scope

- Adding guard-rail existence checks on `mount_device` before the mount
  call. The fix makes the selection correct; runtime existence is btrfs's
  problem.
- Refactoring the "unknown" sentinel out of the expression. Keeping it
  preserves the defensive shape; the comment above justifies why it is
  unreachable today.
- Reordering `BTreeMap` iteration or changing `membership.disks`'s
  container type. Probe order (and therefore degraded-error message order)
  must remain stable — the existing
  `format_degraded_refused_mixed_reasons_enumerates_each_disk_in_order`
  test pins this.
