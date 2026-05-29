# Collapse `TemperatureDiskId` to `LuksUuid`

## Context

`TemperatureDiskId` (cli/src/tui/model.rs:194) is a two-variant enum
(`LuksUuid` / `ByIdPath`) used purely as a hashable key for session
temperature watermarks. After the LUKS-UUID-identity migration (decision
024), the `ByIdPath` variant is unreachable:

- `PoolMembership` is keyed by LUKS UUID, so every member has a UUID.
- `DiskIdentity::from_membership` (model.rs:70-98) builds the `by_id` and
  `luks_uuid` maps from the same `members` iterator, both keyed by name --
  identical key sets.
- The temperature loop (probe.rs:252) iterates `by_id` keys and looks them
  up in `luks_uuid`; the lookup is therefore always `Some`. The `None =>
  TemperatureDiskId::ByIdPath(ByIdPath::parse(..).expect(..))` arm
  (probe.rs:267-269) can never fire.

This is dead code plus a stale doc comment that asserts a runtime "by-id
fallback" contradicting decision 024. No existing test exercises the arm:
no probe test mocks a smartctl temperature, so the loop's temperature
branch never runs in today's tests -- the producer test added below
(Tests) closes exactly that gap. The enum is never pattern-matched, never
serialized, and has no `Display` impl -- it is only a map key, and
`LuksUuid` already derives `Clone, Debug, PartialEq, Eq, Hash`.

**Outcome:** delete the enum entirely and use `LuksUuid` directly as the
reading identity and watermark-map key. One type instead of a one-real-
variant enum; the unreachable parse/expect goes away.

## Approach

Drop `TemperatureDiskId` and replace every use with `LuksUuid`. Scope is
confined to `cli/src/tui/` (model, probe, view, app). No code outside the
TUI references the type; nothing persists or renders it.

### cli/src/tui/model.rs

1. Delete the `TemperatureDiskId` enum and its doc comment (189-197).
2. `TemperatureReading.id` (line 203): `TemperatureDiskId` -> `LuksUuid`.
   Add a one-line field doc preserving the rationale the deleted enum doc
   carried: the id is the stable LUKS-UUID key so session watermarks
   survive device-path / name changes on unplug/replug.
3. `Model.session_temperature_stats` (line 334):
   `HashMap<TemperatureDiskId, TemperatureWatermark>` ->
   `HashMap<LuksUuid, TemperatureWatermark>`.
4. Top import (line 10): drop `ByIdPath` ->
   `use crate::types::{LuksUuid, MountPoint};`. After the enum is gone,
   `ByIdPath`'s only remaining use is the `by_id()` test helper
   (469-471), so add `ByIdPath` to the `#[cfg(test)] mod tests` imports
   (merge into line 463: `use crate::types::{ByIdPath, DiskName};`).
   Leave the `by_id()` helper itself in place -- it builds `DiskIdentity`
   fixtures and is unrelated to this type.

### cli/src/tui/probe.rs

5. Rewrite the temperature block (264-271) to use the UUID directly and
   drop the unreachable `ByIdPath` arm. Flatten into a single guard that
   matches the function's existing "skip on parallel-map miss" idiom (cf.
   the devid loop at probe.rs:308 `let Some(..) = .. else { continue; }`).
   The miss is impossible (decision 024) and the failure mode is benign
   (one disk's temperature omitted for one read-only probe tick), so skip
   rather than panic -- no `expect`/`unreachable` reintroduced:

   ```rust
   // Every membership disk is keyed by its LUKS UUID (decision 024), so a
   // reading's identity is always that UUID. If a probe-only entry somehow
   // lacks one, skip it rather than fabricate identity from the by-id path.
   if let (Some(celsius), Some(uuid)) =
       (probe.celsius, disks.luks_uuid.get(disk_name.as_str()))
   {
       disk_temperature_readings
           .insert(disk_name.clone(), TemperatureReading { id: uuid.clone(), celsius });
   }
   ```

   The `smart_health.insert(...)` above this block is unchanged (runs for
   every disk regardless of temperature).
6. Import (line 22): drop `TemperatureDiskId` from the
   `crate::tui::model::{...}` list; keep `TemperatureReading`. Leave the
   `crate::types` import (line 25) as-is -- `ByIdPath` is still used at
   probe.rs:344 and `LuksUuid` is now used by the block above.

### cli/src/tui/view/mod.rs

7. `temperature_cell` signature (line 786): `stats: &HashMap<LuksUuid,
   TemperatureWatermark>`. The body's `stats.get(&r.id)` (792) is
   unchanged -- `r.id` is now `&LuksUuid`, still a valid key.
8. Import (line 18-22): drop `TemperatureDiskId` from the
   `crate::tui::model::{...}` list. Add a module-level
   `use crate::types::LuksUuid;` (no `crate::types` import exists at the
   top today; `LuksUuid` is now used by the non-test signature).
9. Test fixture (1727-1759): replace `TemperatureDiskId::LuksUuid(uuid(..))`
   with `uuid(..)` at the four sites. The test-local
   `use crate::types::LuksUuid;` (1718) becomes redundant with the new
   module-level import -- remove it.

### cli/src/tui/app.rs (test-only, plus one no-op confirmation)

10. Test import (line 773): `use crate::tui::model::TemperatureReading;`
    (drop `TemperatureDiskId`). Keep `use crate::types::LuksUuid;` (774),
    used by the `temp_uuid` helper.
11. Test constructions (785, 807, 826, 854, 887): replace
    `TemperatureDiskId::LuksUuid(temp_uuid(..))` with `temp_uuid(..)`.
12. No change at line 236: `model.session_temperature_stats
    .entry(reading.id.clone())` -- `reading.id` is now `LuksUuid`, a valid
    key for the re-typed map.

## Tests

This change rewrites the real producer of `TemperatureReading` (the
probe.rs temperature block), so it needs producer-side coverage in
addition to the existing consumer tests.

### New (required): probe-side producer test in cli/src/tui/probe.rs

Add one `probe_pool_for_tui` unit test alongside the existing
mounted-pool probe tests. Preamble per Test Conventions:

- Intent: a mounted-pool probe whose smartctl returns
  `temperature.current` produces a `disk_temperature_readings` entry
  whose `id` is the member's LUKS UUID and whose `celsius` is the
  reported value.
- Why it exists: this is the only path that builds `TemperatureReading`
  from live tool output and the only code the rewrite touches. The
  consumer tests use hand-built readings, so without this a regression
  that drops the reading (e.g. the new `if let (Some, Some)` guard
  wrongly skipping) or assigns the wrong identity would leave the live
  TUI Temp column blank while every existing test still passes.
- Scenario: a single mounted disk `toshiba` reports 38 C via SMART; the
  operator expects its temperature -- and, across ticks, its watermark
  -- tracked by stable LUKS UUID, not by device path.

Recipe (verified against the code):

- Build on the mounted-pool mock pattern (extend
  `one_disk_mounted_pool_runner`, probe.rs:1834) and chain
  `.with_output(CmdRequest::SmartctlHealthJson { device:
  "/dev/disk/by-id/braid-toshiba".to_owned() }, ok_raw("smartctl",
  r#"{"smart_status":{"passed":true},"temperature":{"current":38}}"#))`.
  `device` is a `String` (cmd.rs:180); `parse_smartctl` reads
  `temperature.current` into `SmartProbe.celsius`
  (cli/src/parse/smartctl.rs:161-164).
- Drive the temperature loop (it iterates `disks.by_id`) with
  `tui_disks_with_by_id({"toshiba": "/dev/disk/by-id/braid-toshiba"})`;
  that helper keeps `luks_uuid["toshiba"] =
  11111111-1111-1111-1111-111111111111`, so the smartctl device path and
  the membership UUID line up.
- Assert `pool.disk_temperature_readings["toshiba"].celsius == 38` and
  `...["toshiba"].id ==
  LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap()`.
  This pins both that the reading is produced and that its identity is
  the membership UUID -- the two ways the rewrite could silently break.

### Existing (unchanged, still pinning the consumer side)

The watermark tests in app.rs (seed / widen-max / no-narrow-min /
sample-count / reset, ~800-908) and `snapshot_temperature_column` in
view/mod.rs use only the `LuksUuid` form today; after the mechanical
substitution they continue to pin seeding, hi/lo widening, and the
`sample_count >= 2` render gate. They need no behavioral change -- only
the `TemperatureDiskId::LuksUuid(x)` -> `x` construction edits already
listed in the Approach section.

## Verification

1. `just test-rust` -- compiles lib + tests and runs the cargo suite,
   including the new probe-side producer test, the app.rs watermark
   tests, and the view snapshot test.
2. Watch the compile for `unused_imports` on `TemperatureDiskId` /
   `ByIdPath` / `LuksUuid` in all four files -- a leftover warning means an
   import move above was missed (the model.rs `ByIdPath` -> test-module
   move and the view/mod.rs module-level `LuksUuid` add are the two that
   matter).
3. `snapshot_temperature_column` output must remain byte-identical: only
   fixture *construction* changes, not rendered values, so the snapshot
   should pass without re-acceptance. A snapshot diff here is a red flag,
   not an expected update.

## Out of scope

- Do not re-key `disk_temperature_readings` (HashMap<String,
  TemperatureReading>, name-keyed) -- the view looks up readings by name
  per row, and the `id: LuksUuid` field is the deliberate bridge into the
  session-stats map. Keep both.
- The historical impl note plans/impl/2026-04-16-tui-disk-temperature-
  column.md mentions the old `id: TemperatureDiskId` shape; it is a
  point-in-time record, not authoritative code, and is intentionally left
  untouched.

## Implementation notes

- Producer-test coverage diverged from the plan's "add a new probe-side
  producer test." The plan's premise -- "no probe test mocks a smartctl
  temperature, so the loop's temperature branch never runs in today's
  tests" -- was false against current code: `smartctl_health_for_present_member_uses_live_underlying`
  (probe.rs) already mocks `temperature.current` and asserts the produced
  `celsius`. The plan's new-test recipe also mocked the wrong device (the
  by-id path `/dev/disk/by-id/braid-toshiba`); a mounted member is probed
  via its live underlying path (`/dev/vda`, from cryptsetup status ->
  `mounted_classification` at probe.rs:253-256), so the recipe would have
  produced no reading and failed its own `celsius == 38` assertion. The
  real, narrow gap was that no test asserted `reading.id` (the LUKS-UUID
  identity). Resolved (with the user, "Option 1") by adding the missing
  `reading.id == LuksUuid` assertion plus a widened preamble to that
  existing test, rather than cloning its full mounted-pool setup into a
  near-duplicate new test.
