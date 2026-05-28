# Plan: present-device hardware queries must use the live `underlying`, not persisted `by_id`

## Context

A code-review finding (Low/Correctness) flagged `cli/src/status.rs:985-986`: verbose
`braid status` queries lsblk Model/Serial for a *present* pool device off the persisted
`member.by_id` instead of the live backing path `pd.underlying` (the path `cryptsetup
status` reported for the currently-open mapper).

Per Decision 024 (`docs/design/decisions/024-luks-uuid-identity.md`), `ByIdPath` is a
non-identity hardware address that "may drift (enclosure/controller changes)" -- it is
"Setup and repair only," not persistent. `pd.underlying` is the live, currently-mapped
backing path. Keying a live hardware query off the drift-prone persisted by-id blanks
Model/Serial to `(unknown)` for a healthy, present disk precisely in the drift scenario
the UUID migration was designed to tolerate. Diagnostic-only, never fatal.

Investigation showed this is **one instance of a class**. The codebase already has an
established, consistent convention -- present/mapped disk -> live `underlying`; not-yet-mapped
disk -> `by_id` -- followed by `remove.rs:264` (`work_plan.target_underlying`) and
`replace.rs:448-456` (resolves `pool.devices...underlying` by UUID). Three present-device
hardware queries violate it:

| Callsite | Query | Live path source (currently unused) |
| --- | --- | --- |
| `status.rs:985-986` | lsblk Model/Serial | `pd.underlying` (already in hand) |
| `tui/probe.rs:252-261` | smartctl health/temperature | `mounted_classification` map (built at `:162-169`) |
| `doctor.rs:1168-1200` | smartctl self-test log | `ctx.pool_state` via `ensure_pool_state()` |

No existing test catches any of these (status fixtures happen to make `by_id == underlying`-shaped
mocks pass with `None`; TUI smartctl is untested; doctor self-test tests establish no present
pool device, so they fall back to `by_id`). The ideal fix corrects all three, gives the rule a
single named home, adopts it at the one place that inlines the same lookup, documents the
convention, and adds a regression test per callsite that pins the live-path behavior.

Outcome: a present disk's Model/Serial/SMART data renders correctly even when its persisted
by-id has drifted, and the "use the live backing path for present-device hardware queries"
rule is encoded once and reused.

## Out of scope (deliberate)

- **`tui/browse/state.rs:1039` smartctl picker** -- a manual, interactive device-selection
  menu (`populate_smartctl_devices`) that lists members by `by_id` for the operator to pick.
  Display/selection handle, not an automated live query; the operator sees a failure directly.
  Leave as-is.
- **doctor self-test *hint* text** (`smart_selftest_hint(by_id)`, `doctor.rs:1143/1158`). Only
  the smartctl *query device* changes to `underlying` for present members; the operator-facing
  hint keeps referencing `by_id` (the setup/repair vocabulary per Decision 024). Unchanged.

## Changes

### 1. Shared accessor: `PoolState::underlying_for_uuid` (`cli/src/types.rs`)

Add to the existing `impl PoolState` (`types.rs:448`):

```rust
/// Live backing path for a present pool device identified by LUKS UUID.
/// Hardware queries (lsblk model/serial, smartctl) must prefer this over the
/// persisted, drift-prone `by_id` (Decision 024); callers fall back to `by_id`
/// only when the member is absent and has no live mapping.
pub fn underlying_for_uuid(&self, uuid: &LuksUuid) -> Option<&str> {
    self.devices
        .iter()
        .find(|d| d.luks_uuid == *uuid)
        .map(|d| d.underlying.as_str())
}
```

`LuksUuid` already supports `==` (used in `replace.rs:452`); `PoolDevice` carries
`luks_uuid` + `underlying` (`types.rs:469-479`). Present devices always have a real
`underlying` (the `BackingDevice::Null` case is diverted to `null_underlying` in
`probe.rs:443-451`), so this never returns an empty path for a present device.

### 2. `status.rs` -- the finding (`cli/src/status.rs:985-986`)

In `build_disk_reports`, the present-device loop already binds `pd`. Query lsblk off the
live path:

```rust
// Present-device hardware comes from the live backing path, not the
// persisted (drift-prone) by-id -- see PoolState::underlying_for_uuid /
// Decision 024.
let model = get_lsblk_field(runner, &pd.underlying, LsblkFieldKind::Model);
let serial = get_lsblk_field(runner, &pd.underlying, LsblkFieldKind::Serial);
```

Keep the existing `by_id` binding (`:978-980`) -- it is still the correct value for the
*displayed* address field in `DiskReport.by_id` (`:1006`) and `HumanDisk.by_id` (`:1017`).
Uses `pd.underlying` directly rather than the helper (no indirection -- `pd` is in hand).

### 3. `tui/probe.rs` -- smartctl health/temperature (`cli/src/tui/probe.rs:252-261`)

The smartctl loop iterates `disks.by_id` and already has `mounted_classification`
(`name -> (DiskLockState, Option<underlying>)`, built at `:162-169`). Prefer the live
backing path for a present (unlocked) member; fall back to `by_id` otherwise:

```rust
for (disk_name, by_id_path) in &disks.by_id {
    // Present members carry a live backing path in mounted_classification;
    // the persisted by-id is the drift-prone fallback (Decision 024) used
    // for absent / null-underlying disks.
    let query_device = mounted_classification
        .get(disk_name)
        .and_then(|(_, underlying)| underlying.as_deref())
        .unwrap_or(by_id_path.as_str());
    let probe = runner
        .run(&CmdRequest::SmartctlHealthJson { device: query_device.to_owned() })
        ...
}
```

The temperature-ID assignment (`:264-271`, `TemperatureDiskId::LuksUuid` / `ByIdPath`) is
the reading's *identifier*, not the query path -- leave it unchanged. `null_underlying`
members map to `Some((Unlocked, None))` -> `as_deref()` yields `None` -> by-id fallback
(correct: no live backing exists).

### 4. `doctor.rs` -- smartctl self-test (`cli/src/doctor.rs:1168-1200`)

`check_smart_selftests` iterates `membership.iter_by_name()`, which yields
`(&LuksUuid, &DiskMember)` (`membership.rs:312`) -- currently the UUID is discarded
(`for (_, member)`). Use it to resolve the live path, with a by-id fallback:

```rust
let membership = match load_membership_or_check_result(ctx, NAME) { ... };
ensure_pool_state(ctx);
let live = ctx.pool_state.as_ref().and_then(|r| r.as_ref().ok()); // Option<&PoolState>

let mut checks = Vec::new();
for (uuid, member) in membership.iter_by_name() {
    let subject = member.name.as_str();
    let by_id = member.by_id.as_str();
    let query_device = live
        .and_then(|p| p.underlying_for_uuid(uuid))
        .unwrap_or(by_id);
    let raw = match ctx.runner.run(&CmdRequest::SmartctlSelftestLogJson {
        device: query_device.to_owned(),
    }) { ... };
    checks.push(summarize_smart_selftest(subject, by_id, parse_smartctl_selftest_log(&raw)));
}
```

Notes:
- `ensure_pool_state` (`:638-654`) is defensive: it leaves `pool_state = None` unless config
  is present **and** mounted, so absent-pool / unmounted contexts fall back to `by_id` exactly
  as today. It is cached, so checks that already loaded pool state pay nothing.
- `summarize_smart_selftest` still receives `by_id` -- only the *query device* changes; the
  hint stays by-id (see Out of scope).
- Borrow check: `membership` is owned (loaded from `paths`); `live` (shared `&ctx.pool_state`)
  and `ctx.runner` (shared `&ctx.runner`) are distinct-field shared borrows that coexist.

### 5. Adopt the helper in `replace.rs` (unification, `cli/src/replace.rs:448-453`)

The `ReplaceSource::Live` arm already inlines exactly this lookup:

```rust
ReplaceSource::Live { .. } => pool
    .devices.iter().find(|d| d.luks_uuid == old_uuid).map(|d| d.underlying.as_str()),
```

Replace with `ReplaceSource::Live { .. } => pool.underlying_for_uuid(&old_uuid),`. Same
behavior, gives the helper a second production consumer, removes a hand-rolled copy.

### 6. Document the convention (`docs/design/decisions/024-luks-uuid-identity.md`)

Add one short bullet under the benefits/handles section codifying the rule so future code
and reviewers have a citation: *"Present-device hardware queries (lsblk model/serial,
smartctl) use the live backing path (`PoolState::underlying_for_uuid`), never the persisted
by-id, which may have drifted while the disk is still present."* (CLAUDE.md: behavior/
invariant changes must update the design docs.)

## Test plan

Behavioral, structure-insensitive regression tests -- one per fixed callsite -- each
constructed so it **fails on the old by-id query and passes on the new underlying query**
(present device whose `underlying != by_id`, with a misleading by-id mock):

- **status** (`cli/src/status.rs` `mod tests`): new test
  `present_disk_hw_queried_off_live_underlying_not_by_id`. Build status with the 3-disk
  verbose runner; register an lsblk MODEL mock at the by-id path returning a WRONG value and
  the correct value at the `underlying` path (`/dev/vda`); assert the present disk's
  `HumanDisk.model` is the underlying value. Use `status_runner_healthy_3disk_verbose` /
  `build_disk_reports` as the harness.
- **status fixture re-key** (`cli/src/test_fixtures/status.rs:538-579`): re-key the 6
  `LsblkField` Model/Serial mocks from `/dev/disk/by-id/disk{1,2,3}` to `/dev/vda|vdb|vdc`
  (the `underlying` paths the code now queries). Keeps the shared fixture honest; no other
  test asserts these values, so none break.
- **tui** (`cli/src/tui/probe.rs` `mod tests`): new test asserting smartctl health/temperature
  for a present member is read from the live backing path. Follow the existing
  `probe_pool_for_tui` test setup (mounted pool, `CryptsetupStatus` -> `/dev/vdb`,
  UUID-keyed `DiskIdentity`); mock `SmartctlHealthJson { device: "/dev/vdb" }` -> Passed +
  temperature and `{ device: <by_id> }` -> Unknown/no-temp; assert the result reflects the
  `/dev/vdb` mock. (No existing smartctl mocks to re-key; unmocked smartctl already tolerated.)
- **doctor** (`cli/src/doctor.rs` `mod tests`): new test asserting the self-test query for a
  *present* member uses `underlying`. **The ctx must establish a mounted pool**, or
  `ensure_pool_state` leaves `pool_state = None` and the query falls back to `by_id` -- so the
  test would pass for the wrong reason and never exercise the fix. Do **not** reuse
  `selftest_results_for` / `parsed_doctor_ctx`: that path builds the ctx via
  `for_test_parsed`, which uses `REAL_FILESYSTEM_FOR_TESTS`, so the isolated tmp mount point is
  not mounted btrfs and `ensure_pool_state` no-ops. Instead build the ctx the way the existing
  live-pool doctor tests do (e.g. `doctor.rs:3537-3539`, `:5045-5046`):

  ```rust
  let (dir, paths) = isolated_paths();
  // seed 1 -> test_uuid(1), matching the present pool device below
  save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(1))]);
  let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[])
      .with_output(
          CmdRequest::SmartctlSelftestLogJson { device: "/dev/vdb".to_owned() },      // recent pass
          /* smartctl_selftest_json(...) */)
      .with_output(
          CmdRequest::SmartctlSelftestLogJson { device: "/dev/disk/by-id/disk1".to_owned() }, // fail/stale
          /* smartctl_selftest_json(...) */);
  let fs = DoctorMockFs::mounted_btrfs_only();
  let mut ctx = DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());
  let results = check_smart_selftests(&mut ctx);
  // assert the row reflects the /dev/vdb (recent-pass) result, proving the live path was queried
  ```

  The membership `seed = 1` derives the same UUID as `pool_state_runner`'s `test_uuid(1)`
  (`save_doctor_membership` -> `disk_member_with(seed, ...)`), so the member resolves to the
  present device with `underlying = /dev/vdb`. Confirm existing self-test tests
  (`check_smart_selftest_*`, `single_selftest_*`) still pass unchanged -- they build the ctx
  through `parsed_doctor_ctx` (real, unmounted fs), so `pool_state` stays `None` and they fall
  back to by-id exactly as today.

## Verification

- `just test-rust` -- exercises status/doctor/tui/replace unit tests and the parser golden
  fixtures. Primary gate for this change.
- Targeted while iterating: `just test-rust` then filter, e.g.
  `cargo test -p braid-cli smart_selftest`, `... present_disk_hw`, `... probe_pool_for_tui`
  (run via `just test-rust` per CLAUDE.md; package is `braid-cli`).
- No NixOS VM tests required: the change is pure CLI logic with no systemd/mount/lock blast
  radius. (Parser-critical tool versions unchanged -- no fixture-capture obligation.)
- Do not run `cargo fmt` (CLAUDE.md); keep edits narrow.
