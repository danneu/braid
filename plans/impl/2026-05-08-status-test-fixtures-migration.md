# Plan: Migrate `cli/src/status.rs` test scaffolding to a shared `test_fixtures::status` module

**Status: Draft**

## Context

`cli/src/status.rs` is ~4225 lines. The `#[cfg(test)] mod tests` block at lines 1122-4224 holds 74 tests plus ~525 lines of inline scaffolding: a local `MockFs` (1130-1183) with `new`/`not_mounted`/`ext4` constructors, `ok_raw` (1185), `err_raw` (1194), `mp` (1203), `report_with_scrub` (1207), four btrfs-show factories at 1229/1238/1249/1260, `cryptsetup_status_active` (1272), `cryptsetup_uuid_ok` (1285), two `btrfs_df_*` factories at 1292/1305, `btrfs_usage_raw` (1319), two `btrfs_device_usage_raw_*` at 1332/1361, five `btrfs_scrub_*` factories at 1374/1381/1394/1407/1420, `btrfs_device_stats_3disk` (1433), `lsblk_field_ok` (1444), `test_paths` (1448), two configs at 1454/1458 (byte-identical -- both call `Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()`), `membership_1disk` (1462), the two composite topology runners `runner_healthy_3disk_base` (1472) and `runner_healthy_3disk_verbose` (1549), `fs_3disk` (1631), `fs_1disk` (1642), then deeper in the file `pool_empty` (3450), `cfg_present_not_luks` (3462), `is_luks_raw` (3470), `luks_dump_text_raw` (3479), `report_with_alerts` (3788), `disk_report_named` (3808).

The 74 tests cluster into thirteen behavior families:

- **JSON envelope** (8, lines 1650-2002) -- `status_json_not_mounted`, `not_mounted_status_envelope_is_minimal` (intent preamble at 1706-1717: exactly four keys, no leakage of mounted-only fields), `status_json_healthy`, `status_json_degraded`, `status_json_verbose_disks`, `status_json_disks_always_array_not_mounted`, `status_json_disks_always_array_empty`, `status_json_disks_always_array_verbose`. Pin exact `serde_json::Value` shapes.
- **Human output** (5, lines 2050-2298) -- `status_human_not_mounted`, `status_human_healthy_single`, `status_human_healthy_raid1`, `status_human_degraded`, `status_human_degraded_plural`. Pin exact substring snippets ("DEGRADED (1 missing device)", "Allocation:", "Drives", etc.).
- **Verbose disk rendering** (5, lines 2303-2561) -- `status_verbose_present_disks`, `status_verbose_missing_disk`, `status_verbose_luks_header_unreadable_disk` (intent at 2396-2406: "LUKS HEADER UNREADABLE", "braid doctor" not "braid replace"), `status_verbose_luks_header_damaged_disk` (intent at 2457-2468), `status_verbose_lsblk_failure`. Plus `status_verbose_unknown_disk` (3731, intent at 3720-3729: "UNKNOWN", "metadata unavailable", no LUKS-header label, no doctor hint).
- **Scrub report** (15, lines 2567-2817) -- 5 integration parsing tests via `get_scrub_report` (`status_scrub_finished`, `status_scrub_finished_with_errors`, `status_scrub_aborted`, `status_scrub_interrupted`, `status_scrub_failure_tolerant`); 6 pure JSON serialization tests (`scrub_report_json_*`); 4 pure human rendering tests (`human_scrub_*`).
- **Balance report** (10, lines 2822-3037) -- 4 parsing tests (`balance_report_idle`, `balance_report_running`, `balance_report_paused`, `balance_report_unknown_on_cmd_error`); 2 paused-warning emission tests (`emit_paused_balance_warning_*`); 4 pure human rendering tests (`balance_human_*`, `balance_human_idle_no_line`).
- **Capacity** (1 integration + 6 pure, lines 3078, 3862-3903) -- `get_capacity_raid1_used_is_logical` plus 6 `estimate_pool_capacity_*` cases. The `estimate_*` tests are pure value-in/value-out -- no runner, no fs.
- **Error policy** (5, lines 3039-3176) -- `status_df_failure_fatal`, `status_usage_failure_fatal`, `status_device_stats_failure_fatal`, `status_not_btrfs_maps_to_not_mounted`, plus the capacity check above.
- **`cmd_status` integration** (5, lines 3185-3444) -- `cmd_status_not_mounted_ok`, `cmd_status_healthy_ok`, `cmd_status_healthy_json_ok`, `cmd_status_degraded_ok`, `cmd_status_single_disk_ok`, plus `build_status_missing_devids_unions_btrfs_missing_and_null_underlying` (intent at 3311-3340: missing_count includes null-underlying; missing_devids = `{btrfs MISSING}` U `{null-underlying}`).
- **`build_disk_reports` PresentNotLuks classification** (5, lines 3498-3650) -- `..._probe_failed_falls_back_to_unknown` (intent at 3488-3497), `..._unreadable_maps_to_luks_header_unreadable` (3509-3518), `..._damaged_maps_to_luks_header_damaged`, `..._inconsistent_falls_back_to_unknown`, `..._skips_unpooled_row_when_mapper_in_pool_for_present_not_luks`.
- **Compact drives** (1, line 3699) -- `status_compact_missing_disk`. Pure: derives missing status from `missing_count`.
- **Alert rendering** (3, lines 3821-3856) -- `alert_missing_device_shows_name`, `alert_btrfs_errors_shows_name`, `alert_unknown_devid_falls_back`. Pure: takes a `StatusReport` and renders.
- **Corrupt pool.json regression** (2, lines 3909-3964) -- `cmd_status_corrupt_membership_returns_error` (intent at 3910-3917: mounted + corrupt -> `StatusError::Membership(Corrupt(..))`), `cmd_status_unmounted_corrupt_membership_returns_ok` (intent at 3939-3947: offline ignores it).
- **Mapper conflict + devid pairing + alert latch** (3, lines 3981, 4131, 4206) -- `status_surfaces_mapper_conflict` (intent at 3966-3980: `ProbeError::MapperConflict` propagates as `StatusError::Probe(MapperConflict)`), `disk_report_pairs_stats_by_devid_when_path_differs` (intent at 4113-4130: stats row devid 1 + path "/dev/dm-0" pairs to pool device with devid 1 even when paths mismatch), `resolve_alert_state_surfaces_corrupt_latch_as_computation_error` (corrupt `alert_latch.json` -> active alert with `ComputationError`).

Twenty-plus of these tests carry load-bearing invariants the migration must preserve byte-for-byte:

- **Exact JSON field omission / inclusion.** `status_json_not_mounted` / `not_mounted_status_envelope_is_minimal` assert the offline envelope has exactly four keys (`mount_point`, `status`, `disks`, `alert_active`). `status_json_disks_always_array_*` assert `disks` is always a JSON array, never an object, regardless of presence/empty/populated. `status_json_healthy` / `status_json_degraded` / `status_json_verbose_disks` pin `present_count`, `missing_count`, `profile`, `capacity`, `last_scrub`, `allocation`, `disks` shapes. Any topology that adds a probe whose result mutates a field (e.g. forces a `last_scrub` into the report) would silently shift the envelope.
- **Exact human-output snippets.** "DEGRADED (1 missing device)" / "DEGRADED (2 missing devices)" plural switch (2215, 2268). "MISSING", "not found", "device absent" verbose missing-disk wording (2354). "LUKS HEADER UNREADABLE" / "LUKS HEADER DAMAGED" + "braid doctor" (not "braid replace") verbose disk wording (2407, 2469). "UNKNOWN", "metadata unavailable" non-alarming wording (3731). "(no errors)", "(3 errors)", "cancelled (will resume)", "interrupted" scrub-line wording (2754-2816). "Balance: running, 108/160 chunks (68% complete)", "Balance: unknown" balance-line wording (2944, 2980); idle balance suppressed (3008). Alert wording "missing device: <name> (devid N)", "btrfs device errors on <name> (devid N)" (3821, 3836).
- **Deliberate empty `MockRunner` / missing-mock contracts.** `build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown` (3498) builds `MockRunner::default()` and expects `DiskStatus::Unknown` because no `CryptsetupIsLuks` / `CryptsetupLuksDumpText` outputs are configured. A broad topology installer that auto-resolves these would silently flip the result to `LuksHeaderUnreadable` or `LuksHeaderDamaged`.
- **`/proc/self/mountinfo` semantics across three states.** Status reaches `/proc/self/mountinfo` through `mount_check::fstype_at_mount_via_fs` (`mount_check.rs:172`), distinguishing (a) `/mnt/storage` mounted with `btrfs` -> proceed to query the pool, (b) `/mnt/storage` not mounted -> emit `StatusCode::NotMounted` envelope, (c) `/mnt/storage` mounted with non-btrfs (e.g. `ext4`) -> also emit `NotMounted` (verified by `status_not_btrfs_maps_to_not_mounted` at 3165). The local `MockFs` ships three constructors that emit different mountinfo bodies for these three cases. The migration must preserve byte-identical mountinfo bodies for each case, including the `/dev/mapper/disk1` source-device spelling and the `ext4` variant that `shared::MockFs` does not provide.
- **Read-only command semantics.** Status performs no mutations -- no `BtrfsDeviceAdd`, no `BtrfsDeviceRemove`, no `CryptsetupLuksOpen`, no `BtrfsBalanceSingle`. Tests verify status proves state through reads. Any fixture that introduces a state-flip flag (like `replace_done` in `ReplacementPool` or `remove_done` in `RemoveMissingPool`) would model behavior status does not have.
- **Corrupt pool.json semantics.** The two regression tests rely on `std::fs::write(paths.pool_json(), <corrupt-bytes>)` to seed the corruption directly (verified at status.rs:3922, 3953; `StatePaths::pool_json` lives at `state_paths.rs:19`). `cmd_status_corrupt_membership_returns_error` at 3909 then mounts with btrfs and expects `StatusError::Membership(Corrupt(..))`; `cmd_status_unmounted_corrupt_membership_returns_ok` at 3949 leaves the pool unmounted and expects `Ok(())` because status never reads pool.json when it has already concluded the pool is not mounted. The migration cannot abstract the corrupt-write step into a fixture without losing the visible "this is the bytes that break it" intent.
- **Alert latch corruption.** `resolve_alert_state_surfaces_corrupt_latch_as_computation_error` (4206) writes invalid JSON to `paths.alert_latch_json()` (verified at status.rs:4210; `StatePaths::alert_latch_json` lives at `state_paths.rs:35`) and asserts the alert state surfaces `ComputationError`. Same shape as the corrupt-pool.json tests: keep the `std::fs::write` inline.
- **Pure value tests.** `estimate_pool_capacity_*` (3862-3903) take a `Vec<u64>` of disk sizes and assert the returned capacity. No `MockRunner`, no `MockFs`, no `StatusReport`. Migrating them into the fixture surface would only add ceremony.

Outcome: ship `cli/src/test_fixtures/status.rs` as a flat collection of helpers (modeled on `test_fixtures/mount.rs` and `test_fixtures/enroll_key_file.rs` -- no `*Pool` topology installer, no `*ParamsBuilder`, no state-flip flag). Status is read-only and the test surface is dominated by per-test `MockRunner` composition + load-bearing missing-mock / exact-shape contracts; the only meaningful composites worth keeping are `status_runner_healthy_3disk_base` and the `_verbose` extender, which today drive six of the integration tests. Ship a status-scoped `Filesystem` mock inside the new module (a private `MockFs` struct + `Filesystem` impl whose `read_to_string` returns `NotFound` for every path except `/proc/self/mountinfo` -- byte-identical to status's local at 1130-1183). Do NOT reuse `shared::MockFs`: shared's impl also resolves `*/exclusive_operation` reads (`shared.rs:86`), which would weaken status's existing implicit guard against the read paths accidentally touching preflight / sysfs state. The five status-scoped helper functions (`status_fs_mounted` / `status_fs_not_mounted` / `status_fs_ext4` / `status_fs_three_disk` / `status_fs_one_disk`) return `impl Filesystem`, keeping the concrete type private and sidestepping a name collision with the facade-exported `MockFs` type from `shared` (`test_fixtures.rs:106`). Reuse `shared::mock_ok` (byte-identical to status's local `ok_raw`). Reuse `mount::err_raw` via the alias `use crate::test_fixtures::err_raw as status_err_raw;` (byte-identical to status's local `err_raw`; the alias is mandatory because the local `err_raw` survives sub-commits 2-5 for unmigrated tests). Reuse `doctor::isolated_paths` (byte-identical to status's local `test_paths`). Every newly-exported helper carries a `status_` prefix. The prefix is load-bearing for the same two reasons that drove enroll's prefix: (a) it sidesteps facade collisions with the existing `MockFs` type-name re-export from `shared` and with `mount::err_raw` / `mount::ok_raw`; (b) it lets the staged migration import a fixture helper while the same-purpose local function still exists for unmigrated tests, since Rust treats `use foo::bar;` plus a same-named local `fn bar` in the same module as a duplicate-definition error. Migrate tests in six small sub-commits keeping `just test-rust` green at each boundary.

This is unreleased software (AGENTS.md "No backwards compatibility"), so we delete the old scaffolding in the cleanup commit rather than deprecate it.

## Recommended approach

### A. New module `cli/src/test_fixtures/status.rs`

Gated `#[cfg(test)]`; registered in `cli/src/test_fixtures.rs` as a private submodule (`mod status;`) with `#[allow(unused_imports)] pub(crate) use status::{...}` re-exports through the facade -- matching the existing pattern at `test_fixtures.rs:73-106`. Sibling test code imports via the facade only (`use crate::test_fixtures::{status_mp, status_fs_mounted, ...}; use crate::test_fixtures::err_raw as status_err_raw;`). The `err_raw` reuse is mandatory-aliased on import; `mock_ok` and `isolated_paths` are imported bare. All items inside the new module are `pub(crate)` and test-only. **Naming convention: every newly-exported helper carries a `status_` prefix.** Module-level doc comment explains why this scope ships flat helpers (no topology installer, no params builder) -- read-only command semantics plus load-bearing missing-mock contracts plus per-test `MockRunner` composition diversity -- and documents the `status_` prefix decision so a future reviewer doesn't try to "simplify" by stripping the prefix.

Items in the module:

```rust
// ---------------------------------------------------------------------------
// Filesystem (three /proc/self/mountinfo states status distinguishes)
//
// Status-scoped: the new module ships its own private MockFs struct +
// Filesystem impl whose semantics are byte-identical to the local one
// at status.rs:1130-1183 -- read_to_string returns NotFound for every
// path except /proc/self/mountinfo, is_block_device always returns false,
// list_dir always returns the empty vec. Distinct from shared::MockFs::
// storage, which also resolves */exclusive_operation reads (shared.rs:86)
// -- a delta that would weaken status's existing implicit guard against
// the read paths accidentally touching preflight / sysfs state. The
// struct stays private to the module so it does not collide with the
// facade-exported MockFs type from shared (test_fixtures.rs:106). The
// five status_fs_* helper functions return `impl Filesystem` so the
// concrete type never escapes the module.
// ---------------------------------------------------------------------------

// (private to module)
struct MockFs {
    paths: Vec<String>,
    mountinfo: String,
}
impl Filesystem for MockFs { /* byte-identical to status.rs:1164-1183 */ }

pub(crate) fn status_fs_mounted(paths: &[&str]) -> impl Filesystem;
    // /mnt/storage mounted with btrfs; mountinfo body
    // "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/disk1 rw\n".
    // Byte-identical to status's local MockFs::new at 1137.

pub(crate) fn status_fs_not_mounted(paths: &[&str]) -> impl Filesystem;
    // Rootfs only at /; no /mnt/storage entry. Mountinfo body
    // "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n". Byte-identical
    // to status's local MockFs::not_mounted at 1146. Use for offline tests.

pub(crate) fn status_fs_ext4(paths: &[&str]) -> impl Filesystem;
    // /mnt/storage mounted with ext4 (not btrfs); mountinfo body
    // "36 35 0:32 / /mnt/storage rw shared:1 - ext4 /dev/sda1 rw\n".
    // Drives status_not_btrfs_maps_to_not_mounted (3165). Byte-identical
    // to status's local MockFs::ext4 at 1154.

pub(crate) fn status_fs_three_disk() -> impl Filesystem;
    // status_fs_mounted preset with the six paths the 3-disk topology
    // needs: /dev/disk/by-id/disk{1,2,3} and /dev/mapper/disk{1,2,3}.
    // Byte-identical to status's local fs_3disk at 1631.

pub(crate) fn status_fs_one_disk() -> impl Filesystem;
    // status_fs_mounted preset with the two paths the 1-disk topology
    // needs: /dev/disk/by-id/disk1 and /dev/mapper/disk1. Byte-identical
    // to status's local fs_1disk at 1642.

// ---------------------------------------------------------------------------
// Identifier / config / membership primitives
// ---------------------------------------------------------------------------

pub(crate) fn status_mp() -> MountPoint;
    // MountPoint("/mnt/storage"). Replaces local fn mp at 1203.

pub(crate) fn status_config() -> Config;
    // Config::new(MountPoint("/mnt/storage")). Collapses status's two
    // byte-identical locals (config_3disk at 1454, config_1disk at 1458)
    // into one.

pub(crate) fn status_membership_1disk() -> PoolMembership;
    // Single-disk PoolMembership keyed by "disk1" with by-id
    // "/dev/disk/by-id/disk1". Replaces local fn membership_1disk at 1462.

// ---------------------------------------------------------------------------
// btrfs CLI output factories (RawCommandOutput leaf builders)
// ---------------------------------------------------------------------------

pub(crate) fn status_btrfs_show_1disk() -> RawCommandOutput;
pub(crate) fn status_btrfs_show_3disk() -> RawCommandOutput;
pub(crate) fn status_btrfs_show_3disk_1missing() -> RawCommandOutput;
    // 3-disk RAID1 with one device marked MISSING. Drives degraded-pool
    // tests.
pub(crate) fn status_btrfs_show_3disk_1null_underlying_1missing() -> RawCommandOutput;
    // 3-disk RAID1 with one healthy + one null-underlying (path MISSING)
    // + one btrfs MISSING. Drives the missing_devids union test (3341).

pub(crate) fn status_btrfs_df_single() -> RawCommandOutput;
    // Single-profile df JSON for 1-disk pool.
pub(crate) fn status_btrfs_df_raid1() -> RawCommandOutput;
    // RAID1 df JSON for healthy 3-disk pool.

pub(crate) fn status_btrfs_usage_raw() -> RawCommandOutput;
    // raw 'btrfs filesystem usage' text -- device size 1TB, allocated
    // 480MB, used 32MB. Drives the get_capacity tests.

pub(crate) fn status_btrfs_device_usage_raw_1disk() -> RawCommandOutput;
pub(crate) fn status_btrfs_device_usage_raw_3disk() -> RawCommandOutput;

pub(crate) fn status_btrfs_scrub_never() -> RawCommandOutput;
pub(crate) fn status_btrfs_scrub_finished() -> RawCommandOutput;
pub(crate) fn status_btrfs_scrub_finished_with_errors() -> RawCommandOutput;
pub(crate) fn status_btrfs_scrub_aborted() -> RawCommandOutput;
pub(crate) fn status_btrfs_scrub_interrupted() -> RawCommandOutput;
    // Five canonical scrub-status outputs. Each variant is keyed by one
    // of the five integration tests at 2567, 2588, 2613, 2637, 2661.
    // Byte-identical to status's locals at 1374-1431.

pub(crate) fn status_btrfs_device_stats_3disk() -> RawCommandOutput;
    // device-stats JSON for 3-disk (all error counters zero). Used by
    // both runner_healthy_3disk_base and the verbose extender today.

// ---------------------------------------------------------------------------
// cryptsetup output factories
// ---------------------------------------------------------------------------

pub(crate) fn status_cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput;
    // LUKS2 'cryptsetup status' active output. Distinct from mount's
    // helpers (mount asserts on inactive mappers). Pair-shaped is
    // unnecessary -- the request key is plain CryptsetupStatus { mapper }
    // and tests already build the request inline.

pub(crate) fn status_cryptsetup_uuid_ok(device: &str, uuid: &str) -> RawCommandOutput;
    // CryptsetupLuksUuid output. Distinct signature from
    // mount::luks_uuid_ok (mount returns a (CmdRequest, RawCommandOutput)
    // pair; status returns just the RawCommandOutput because
    // runner_healthy_3disk_* builds the request inline).

pub(crate) fn status_is_luks_raw(device: &str, exit: i32, stderr: &str) -> RawCommandOutput;
pub(crate) fn status_luks_dump_text_raw(
    device: &str, exit: i32, stdout: &str, stderr: &str,
) -> RawCommandOutput;
    // PresentNotLuks classification leaf factories. Two of the five
    // tests at 3498-3650 exercise non-zero exits with specific stderr
    // wording ("Device ... is not a valid LUKS device.\n", "Cannot read
    // LUKS header metadata.\n"). Byte-identical to status's locals at
    // 3470, 3479.

// ---------------------------------------------------------------------------
// lsblk output factory
// ---------------------------------------------------------------------------

pub(crate) fn status_lsblk_field_ok(cmd: &str, value: &str) -> RawCommandOutput;
    // Wraps mock_ok with a trailing newline ("{value}\n") -- the same
    // contract as status's local fn lsblk_field_ok at 1444.

// ---------------------------------------------------------------------------
// Composite runners (the only meaningful composites worth keeping)
// ---------------------------------------------------------------------------

pub(crate) fn status_runner_healthy_3disk_base() -> MockRunner;
    // Smallest set of probes for a healthy 3-disk RAID1: BtrfsFilesystemShow
    // (3disk), CryptsetupStatus + CryptsetupLuksUuid for disk1/disk2/disk3
    // mappers, BtrfsFilesystemDfJson (raid1), BtrfsFilesystemUsageRaw,
    // BtrfsDeviceUsageRaw (3disk), BtrfsScrubStatus (never),
    // BtrfsDeviceStatsJson (3disk). Used by the cmd_status integration
    // tests (cmd_status_healthy_*, cmd_status_degraded_ok,
    // build_status_missing_devids_*) plus the JSON-envelope and
    // verbose-rendering tests in sub-commit 3 that drive a full topology.
    // (The disk_report_pairs_stats_by_devid_when_path_differs test at
    // 4131 uses MockRunner::default() and does NOT consume this runner;
    // see sub-commit 2's no-touch note.)
    //
    // Does NOT use MockRunner::with_handler -- only with_output. Tests
    // that deliberately omit a probe to surface MissingMock can layer
    // on top via further .with_output(...) calls without
    // worrying about a broad handler shadowing the seeded outputs.

pub(crate) fn status_runner_healthy_3disk_verbose(runner: MockRunner) -> MockRunner;
    // Extends a base runner with verbose-mode probes: probe_config_disk's
    // CryptsetupLuksUuid for each /dev/disk/by-id/diskN, plus
    // BtrfsDeviceStatsJson (re-seeds the 3-disk stats), plus six
    // LsblkField calls (Model + Serial for each of disk1/disk2/disk3).
    // Used by 3 integration tests today (cmd_status_healthy_ok,
    // status_json_verbose_disks, status_verbose_present_disks's variants
    // that drive a full topology). Two-function form preserved -- some
    // tests want only the base, some want both.

// ---------------------------------------------------------------------------
// Pool / config-disk / report data builders
// ---------------------------------------------------------------------------

pub(crate) fn status_pool_empty() -> PoolState;
    // PoolState { mounted: true, devices: vec![], missing_count: 0, ... }.
    // Used by build_disk_reports tests at 3498-3650 to seed an empty pool
    // before classification.

pub(crate) fn status_cfg_present_not_luks(name: &str, by_id: &str) -> Vec<ConfigDisk>;
    // One-element ConfigDisk vec with state PresentNotLuks. Used by 5
    // PresentNotLuks classification tests.

pub(crate) fn status_report_with_scrub(scrub: ScrubReport) -> StatusReport;
    // Canonical 3-disk RAID1 StatusReport with last_scrub set. Drives 8
    // tests at lines 2670-2820 (scrub JSON serialization + human
    // rendering).

pub(crate) fn status_report_with_alerts(
    disks: Vec<DiskReport>, causes: Vec<AlertCause>,
) -> StatusReport;
    // Degraded StatusReport with alert_active=true and the supplied
    // disks + alert_causes. Drives the three alert-rendering tests at
    // 3821, 3836, 3847.

pub(crate) fn status_disk_report_named(name: &str, devid: u64) -> DiskReport;
    // DiskReport { mapper: "braid-{name}", by_id, devid, status: Present,
    // errors: <none>, ... }. Pairs naturally with status_report_with_alerts.
```

**Reused via existing facade exports (no new declarations):**

- `mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput` (`shared.rs:23`, re-exported at `test_fixtures.rs:106`) -- byte-identical to status's local `ok_raw`. Migrated tests `use crate::test_fixtures::mock_ok;` and call `mock_ok(...)`. **No alias needed**: the local helper is named `ok_raw`, not `mock_ok`, so the bare import does not collide with the still-present local during sub-commits 2-5.
- `err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput` (`mount.rs:81`, re-exported at `test_fixtures.rs:90`) -- byte-identical to status's local `err_raw`. Migrated tests **import via alias**: `use crate::test_fixtures::err_raw as status_err_raw;` and rewrite call sites to `status_err_raw(...)`. The alias is mandatory: a bare `use crate::test_fixtures::err_raw;` would collide with the still-extant local `fn err_raw` in `status.rs::tests` (line 1194) during the staged migration. The alias stays after the local is deleted; do not "simplify" it back.
- `isolated_paths() -> (TempDir, StatePaths)` (`doctor.rs:26`, re-exported at `test_fixtures.rs:78`) -- byte-identical to status's local `test_paths` (1448): both call `tempfile::TempDir::new().unwrap()` + `StatePaths::custom(tmp.path().into())`. Migrated tests `use crate::test_fixtures::isolated_paths;` and call `isolated_paths()`. **No alias needed**: the local helper is named `test_paths`, not `isolated_paths`.

`shared::MockFs` is **not** reused -- see "Outcome" above and the "What does NOT go in this module" bullet below for why a status-scoped FS mock is the right grain. No new addition to `shared.rs` is required by this plan.

**What does NOT go in this module (intentional omissions):**

- **No `StatusPool` / `StatusTopology` handler installer.** Status performs no mutations; there is no state to flip. More importantly, twenty-plus tests (Context's bullet list) pin exact JSON field omission, exact human-output snippets, or deliberate empty-`MockRunner` contracts. A broad `with_handler` would either resolve a probe a `MissingMock` test wants to fail (silent inversion: `Unknown` -> `LuksHeaderUnreadable`) or seed a probe whose result rewrites a JSON field (silent breakage of the envelope-shape assertions). Mirrors `mount.rs`'s decision (`test_fixtures/mount.rs:11-20`), `enroll_key_file.rs`'s (`test_fixtures/enroll_key_file.rs:1-20`), and `recover.rs`'s.
- **No `StatusParamsBuilder`.** Status takes no params struct -- the entry point at `status.rs:421` is `cmd_status<R, F>(runner: &R, fs: &F, config: &Config, json: bool, paths: &StatePaths)` (single `json: bool`, no verbosity flag). No per-test scenario configures a struct; a builder would have nothing to build.
- **No `PoolFixture`.** The two corrupt-pool.json regression tests (3909, 3949) and the alert-latch test (4206) need `paths` (from `isolated_paths()`) plus an inline `std::fs::write` of intentionally-corrupt bytes. `PoolFixture::two_disk_healthy` would write a *valid* pool.json the test then has to overwrite, which is wasted work and obscures the visible "this is the byte sequence that triggers the regression" intent. `isolated_paths()` is the right shape.
- **No state-flip flag (no `Arc<AtomicBool>` for "post-status").** Status reads pool state and produces a `StatusReport`. There is no "before" / "after" output to flip. Modeling one would invent a contract that does not exist.
- **No promotion of pure-value tests.** The 6 `estimate_pool_capacity_*` tests at 3862-3903 are `assert_eq!(estimate_pool_capacity(&[size_a, size_b, ...]), expected);`. No runner, no fs, no `StatusReport`. They use no helpers at all today; the migration leaves them untouched.
- **No promotion of pure JSON serialization / human rendering tests beyond the data-builder swap.** The 6 `scrub_report_json_*` tests at 2673-2751 take a `ScrubReport` literal and `serde_json::to_value` it. The 4 `human_scrub_*` tests at 2754-2816 take a `StatusReport` (built via `status_report_with_scrub`) and render it via `format_human_status` (or equivalent). The 4 `balance_human_*` / `balance_human_idle_no_line` tests are similar. The migration's only touch on these is swapping `report_with_scrub(...)` -> `status_report_with_scrub(...)` in the human-rendering family. The pure-JSON tests don't even call that helper, so they're zero-touch.
- **No promotion of `disk_report_named` / `report_with_alerts` to `shared`.** Each has only the three alert-rendering tests as in-tree consumers. Status-scoped is the right grain.
- **No reuse of `shared::MockFs`.** Shared's `read_to_string` impl (`shared.rs:83-91`) resolves `*/exclusive_operation` reads in addition to `/proc/self/mountinfo`. Status's local `MockFs` (status.rs:1173-1178) deliberately returns `NotFound` for everything except `/proc/self/mountinfo` -- a stricter contract that acts as a passive guard against the read paths reaching `exclusive_operation` or any other sysfs / preflight path it doesn't expect. Reusing `shared::MockFs` would silently relax that guard. The new module ships its own private `MockFs` struct + `Filesystem` impl that preserves the strict semantics byte-for-byte. No `with_mountinfo` setter on `shared::MockFs` is added; no `shared.rs` change is required.
- **No `StatusMockFs` re-export at the facade.** The status-scoped `MockFs` struct stays private to `cli/src/test_fixtures/status.rs`. The five `status_fs_*` helpers return `impl Filesystem`, so the concrete type never crosses the module boundary, and the facade does not have to disambiguate against `shared::MockFs` (re-exported at `test_fixtures.rs:106`).
- **No promotion of the per-disk `cryptsetup_uuid_ok(device, uuid)` to `shared` or to `mount`.** mount's `luks_uuid_ok` returns a `(CmdRequest, RawCommandOutput)` pair (different shape) and embeds a fixed UUID; status threads four different UUIDs through `runner_healthy_3disk_*` and consumes the bare `RawCommandOutput`. Different signatures, different consumers.

### B. Migration ordering principle

Move scaffolding once, then replace local references one family at a time. **Hard cases first, bulk second** -- same principle the recent `enroll_key_file` migration used (`plans/impl/2026-05-08-enroll-key-file-test-fixtures-migration.md`):

- (a) **`cmd_status` integration + the two "structural integrity" tests that consume the topology runner (missing_devids, mapper_conflict) + the two corrupt-pool.json regressions + alert latch + `status_not_btrfs`** are the highest-risk family because they exercise the full topology runner, the three mountinfo states, and the corrupt-on-disk states. If the new fixture's leaf factories or runner composers diverge from today's locals, these tests fail loudly. Migrate first to validate the fixture surface end-to-end before touching the bulk. The third structural-integrity test, `disk_report_pairs_stats_by_devid_when_path_differs` (4131), runs in this same sub-commit for coverage but is **no-touch** -- its setup is literal `MockRunner::default()` + literal struct values with no fixture-eligible scaffolding.
- (b) **JSON envelope + human output + verbose disk rendering** swap together: they share `status_runner_healthy_3disk_*` for the topology-driven cases and `status_disk_report_named` for the literal-`StatusReport` cases. Bulk import-only swap.
- (c) **`build_disk_reports` PresentNotLuks classification + `status_compact_missing_disk` + `status_verbose_unknown_disk`** swap together: they share `status_pool_empty` / `status_cfg_present_not_luks` / `status_is_luks_raw` / `status_luks_dump_text_raw`.
- (d) **Long tail bulk migration**: scrub JSON serialization / human rendering, balance parsing / paused-warning / human rendering, capacity (pure + integration), error policy (df / usage / device-stats fatal), alert rendering. These are the simpler tests -- mostly primitives swap (`ok_raw` -> `mock_ok`, `err_raw` -> `status_err_raw`, `mp` -> `status_mp`, `report_with_scrub` -> `status_report_with_scrub`, etc.). Pure tests that use no helpers (the 6 `estimate_pool_capacity_*`) are untouched.
- (e) **Cleanup**: delete the now-unused locals in one mechanical pass.

### C. Migration table

| Sub-commit | Action | Validates |
|---|---|---|
| 1 | Land `cli/src/test_fixtures/status.rs` with the items in §A (every newly-exported name carries the `status_` prefix). The new module ships its own private `MockFs` struct + `Filesystem` impl with byte-identical semantics to `status.rs:1130-1183` (read_to_string returns NotFound for every path except /proc/self/mountinfo); the five `status_fs_*` helpers return `impl Filesystem`. **No `shared.rs` change** -- the status MockFs is local to the module. Register `mod status;` (private) + `#[allow(unused_imports)] pub(crate) use status::{...}` facade re-exports in `test_fixtures.rs` (matching the existing groups at lines 73-106). Mark every item in the new module `#[allow(dead_code)]` since no consumers yet. Do **not** add a new `err_raw` re-export -- mount's existing `err_raw` (`test_fixtures.rs:90`) is byte-identical and already in the facade; consumers in sub-commits 2-5 import it via the alias `use crate::test_fixtures::err_raw as status_err_raw;`. For `mock_ok` (already at `test_fixtures.rs:106`) and `isolated_paths` (already at `test_fixtures.rs:78`), no alias is needed. **Verify** the audit grep cited in Verification (one-time, before sub-commit 1 lands). Update `test_fixtures.rs` module-level doc comment to mention the new scope and to record the `status_` prefix decision (one bullet, mirroring the `enroll_key_file` bullet at lines 37-51). | Module compiles; `cargo check --manifest-path cli/Cargo.toml --tests` clean; `just test-rust` green. |
| 2 | **`cmd_status` integration + structural integrity + corrupt pool.json + alert latch + ext4 -- the highest-risk family, ~11 tests.** Migrate, in this exact order so the runner composers are validated before the corrupt-state tests build on them: `cmd_status_not_mounted_ok` (3185), `cmd_status_healthy_ok` (3196), `cmd_status_healthy_json_ok` (3207), `cmd_status_degraded_ok` (3218), `cmd_status_single_disk_ok` (3372), `build_status_missing_devids_unions_btrfs_missing_and_null_underlying` (3341), `status_surfaces_mapper_conflict` (3981), `cmd_status_corrupt_membership_returns_error` (3909), `cmd_status_unmounted_corrupt_membership_returns_ok` (3949), `status_not_btrfs_maps_to_not_mounted` (3165), `resolve_alert_state_surfaces_corrupt_latch_as_computation_error` (4206). **No-touch in this sub-commit (run for coverage only):** `disk_report_pairs_stats_by_devid_when_path_differs` (4131) -- the test uses `MockRunner::default()` plus literal `PoolState` / `ConfigDisk` / `BtrfsDeviceStatsOutput` values and calls `build_disk_reports` directly; it has no runner / fs / config / paths setup to migrate. Run it for regression coverage but leave the test body untouched. Per-test `use` lines (for the migrated 11) pull `status_runner_healthy_3disk_base`, `status_runner_healthy_3disk_verbose`, `status_fs_three_disk`, `status_fs_one_disk`, `status_fs_not_mounted`, `status_fs_ext4`, `status_mp`, `status_config`, `status_membership_1disk`, `status_btrfs_show_3disk_1missing`, `status_btrfs_show_3disk_1null_underlying_1missing`, `status_btrfs_show_1disk`, `status_btrfs_df_single`, `status_btrfs_device_usage_raw_1disk`, plus the bare reuses `mock_ok` / `isolated_paths` plus the **aliased** reuse `use crate::test_fixtures::err_raw as status_err_raw;`. The migrated tests rewrite `MockFs::new(...)` -> `status_fs_three_disk()` (or the matching wrapper), `MockFs::not_mounted(...)` -> `status_fs_not_mounted(...)`, `MockFs::ext4(...)` -> `status_fs_ext4(...)`, `mp()` -> `status_mp()`, `config_3disk()` / `config_1disk()` -> `status_config()`, `runner_healthy_3disk_base()` / `runner_healthy_3disk_verbose(...)` -> `status_runner_healthy_3disk_base()` / `status_runner_healthy_3disk_verbose(...)`, `test_paths()` -> `isolated_paths()`, `ok_raw(...)` -> `mock_ok(...)`, `err_raw(...)` -> `status_err_raw(...)`. `cmd_status` invocations remain `cmd_status(&runner, &fs, &config, json, &paths)` -- a single `bool` argument before `&paths`, not two. The unmigrated tests keep calling the still-present locals `mp` / `MockFs::*` / `runner_healthy_3disk_*` / etc. without conflict because nothing in the test mod imports those bare names. **Preserve byte-for-byte:** the inline `std::fs::write(paths.pool_json(), ...)` in the corrupt-pool.json tests (status.rs:3922, 3953), the inline `std::fs::write(paths.alert_latch_json(), ...)` in the alert-latch test (status.rs:4210), every `// Intent / Why it exists / Scenario` preamble (3311-3340, 3910-3917, 3939-3947, 3966-3980), every `assert_eq!(...)` / `assert!(matches!(...))` body, every `MissingMock`-deferred mock omission. | `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_*` (5 tests), `... ::build_status_*` (1), `... ::status_surfaces_*` (1), `... ::status_not_btrfs_*` (1), `... ::resolve_alert_state_*` (1), and the no-touch coverage run `... ::disk_report_pairs_stats_by_devid_when_path_differs` (1) all green. Then `cargo test --manifest-path cli/Cargo.toml --lib status::tests`. Then `just test-rust`. The healthy-3-disk runner composer must produce the same `runner.requests()` log shape and same dispatch resolution as today's local. The corrupt-state tests must surface the same `StatusError` variants. |
| 3 | **JSON envelope + human output + verbose disk rendering -- bulk import-only, ~18 tests.** Migrate JSON envelope (8): `status_json_not_mounted` (1650), `not_mounted_status_envelope_is_minimal` (1719), `status_json_healthy` (1742), `status_json_degraded` (1801), `status_json_verbose_disks` (1834), `status_json_disks_always_array_not_mounted` (1946), `status_json_disks_always_array_empty` (1972), `status_json_disks_always_array_verbose` (2002). Human output (5): `status_human_not_mounted` (2050), `status_human_healthy_single` (2076), `status_human_healthy_raid1` (2141), `status_human_degraded` (2215), `status_human_degraded_plural` (2268). Verbose disk rendering (5): `status_verbose_present_disks` (2303), `status_verbose_missing_disk` (2354), `status_verbose_luks_header_unreadable_disk` (2407), `status_verbose_luks_header_damaged_disk` (2469), `status_verbose_lsblk_failure` (2517). Imports cover the previously-unintroduced helpers: `status_btrfs_show_3disk` (the healthy 3-disk show used by JSON-healthy and human-RAID1 tests), `status_btrfs_show_1disk` (single-disk human test), `status_btrfs_df_raid1`, `status_btrfs_usage_raw`, `status_btrfs_device_usage_raw_3disk`, `status_btrfs_scrub_never`, `status_btrfs_device_stats_3disk`, `status_cryptsetup_status_active`, `status_cryptsetup_uuid_ok`, `status_lsblk_field_ok`, `status_disk_report_named` (used by literal-`StatusReport` JSON-verbose-disks and verbose-rendering tests). **Preserve byte-for-byte:** every JSON-key assertion (offline envelope's exactly-four-keys at 1719-1740, `disks` always-array at 1946-2030), every plural / singular missing-device wording switch (2215, 2268), every "LUKS HEADER UNREADABLE" / "LUKS HEADER DAMAGED" / "braid doctor" / "braid replace" snippet (2407, 2469), every "(unknown)" lsblk-failure render (2517). | Per-family runs: `cargo test ... status::tests::status_json`, `... ::status_human`, `... ::status_verbose`. Then full `status::tests`, then `just test-rust`. The pinned snippets (Context's "exact human-output snippets" bullet) must round-trip; the JSON envelope key sets must match today's exact sets. |
| 4 | **`build_disk_reports` PresentNotLuks + compact + verbose-unknown -- ~7 tests.** Migrate: `build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown` (3498), `build_disk_reports_present_not_luks_unreadable_maps_to_luks_header_unreadable` (3519), `build_disk_reports_present_not_luks_damaged_maps_to_luks_header_damaged` (3548), `build_disk_reports_present_not_luks_inconsistent_falls_back_to_unknown` (3591), `build_disk_reports_skips_unpooled_row_when_mapper_in_pool_for_present_not_luks` (3635), `status_compact_missing_disk` (3699), `status_verbose_unknown_disk` (3731). Imports cover `status_pool_empty`, `status_cfg_present_not_luks`, `status_is_luks_raw`, `status_luks_dump_text_raw`, plus any leftover bare-name reuses already pulled in earlier sub-commits. **Preserve byte-for-byte:** the `MockRunner::default()` (no outputs configured) in `build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown` (3501) -- the new fixture must NOT introduce any helper that auto-resolves `CryptsetupIsLuks` / `CryptsetupLuksDumpText` here, or the assertion `assert_eq!(ctx.disks[0].status, DiskStatus::Unknown);` (3506) flips silently to `LuksHeaderUnreadable` and the test passes for the wrong reason. The chained `MockRunner::default().with_output(IsLuks, ...).with_output(LuksDumpText, ...)` shapes at 3551-3568 / 3594-3611 round-trip exactly. The `// Intent / Why ... / Scenario` preamble at 3488-3497 / 3509-3518 / 3539-3547 / 3576-3589 / and the verbose-unknown preamble at 3720-3729 round-trip byte-for-byte. | `cargo test ... status::tests::build_disk_reports_present_not_luks` (5), `status_compact_missing_disk` (1), `status_verbose_unknown_disk` (1). Then full `status::tests`, then `just test-rust`. The probe-failed test must continue to surface `Unknown` and the inconsistent-falls-back-to-unknown test must continue to surface `Unknown` -- if either flips, the migration introduced an unintended auto-resolution. |
| 5 | **Long tail bulk migration: scrub + balance + capacity + error policy + alert rendering -- ~32 tests.** Migrate: scrub integration parsing (5 at 2567-2671) -- `status_scrub_finished`, `status_scrub_finished_with_errors`, `status_scrub_aborted`, `status_scrub_interrupted`, `status_scrub_failure_tolerant`. Scrub JSON serialization (6 at 2673-2751) -- `scrub_report_json_finished`, `_aborted`, `_interrupted`, `_never`, `_running_with_pct`, `_running_no_pct`. Scrub human rendering (4 at 2754-2816) -- `human_scrub_shows_no_errors`, `_shows_error_count`, `_shows_aborted`, `_shows_interrupted`. Balance parsing (4 at 2822-2898) -- `balance_report_idle`, `_running`, `_paused`, `_unknown_on_cmd_error`. Balance paused-warning (2 at 2899-2937) -- `emit_paused_balance_warning_writes_to_buffer`, `_silent_when_idle`. Balance human rendering (3 at 2944-3037) -- `balance_human_running`, `balance_human_unknown`, `balance_human_idle_no_line`. Capacity integration (1 at 3078) -- `get_capacity_raid1_used_is_logical`. Capacity pure (6 at 3862-3903) -- `estimate_pool_capacity_*`. Error policy (3 at 3039, 3051, 3153) -- `status_df_failure_fatal`, `status_usage_failure_fatal`, `status_device_stats_failure_fatal`. Alert rendering (3 at 3821-3852) -- `alert_missing_device_shows_name`, `alert_btrfs_errors_shows_name`, `alert_unknown_devid_falls_back`. Imports cover `status_btrfs_scrub_never` / `_finished` / `_finished_with_errors` / `_aborted` / `_interrupted` (only the integration-parsing tests need them; the JSON / human pure tests don't), `status_report_with_scrub` (scrub human rendering family), `status_report_with_alerts`, `status_disk_report_named` (alert rendering family), `status_btrfs_df_raid1`, `status_btrfs_usage_raw` (capacity integration), `status_btrfs_device_stats_3disk` (device-stats fatal), plus the bare reuses `mock_ok` / `status_err_raw` / `status_mp`. The 6 `estimate_pool_capacity_*` tests **import nothing from the fixture** -- they call `super::estimate_pool_capacity(&[...])` directly. **Preserve byte-for-byte:** every "(no errors)" / "(N errors)" / "cancelled (will resume)" / "interrupted" scrub-line wording (2754-2816), every "Balance: running, 108/160 chunks (68% complete)" / "Balance: unknown" balance-line wording (2944, 2980), every paused-balance hint string (2899-2937), every alert wording at 3821-3852, every fatal-error variant (`StatusError::Df`, `_::Usage`, `_::DeviceStats`) at 3039-3176, every `estimate_pool_capacity` integer assertion (3862-3903). | Per-family runs: `cargo test ... status::tests::status_scrub`, `... ::scrub_report_json`, `... ::human_scrub`, `... ::balance`, `... ::estimate_pool_capacity`, `... ::get_capacity`, `... ::status_df_failure`, `... ::status_usage_failure`, `... ::status_device_stats_failure`, `... ::alert_`. Then full `status::tests`, then `just test-rust`. The five integration scrub-parse tests must produce the same `ScrubReport` variants today's locals do; the four pure scrub-human tests must produce the same string snippets; the six `estimate_pool_capacity_*` tests must produce the same `u64` outputs. |
| 6 | **Cleanup**: delete the now-unused locals in `status.rs::tests`: `MockFs` struct + ctors + `Filesystem` impl (1130-1183), `ok_raw` (1185), `err_raw` (1194), `mp` (1203), `report_with_scrub` (1207), `btrfs_show_1disk` / `_3disk` / `_3disk_1missing` / `_3disk_1null_underlying_1missing` (1229-1270), `cryptsetup_status_active` (1272), `cryptsetup_uuid_ok` (1285), `btrfs_df_single` / `_raid1` (1292, 1305), `btrfs_usage_raw` (1319), `btrfs_device_usage_raw_3disk` / `_1disk` (1332, 1361), `btrfs_scrub_never` / `_finished` / `_finished_with_errors` / `_aborted` / `_interrupted` (1374-1431), `btrfs_device_stats_3disk` (1433), `lsblk_field_ok` (1444), `test_paths` (1448), `config_3disk` / `config_1disk` (1454, 1458), `membership_1disk` (1462), `runner_healthy_3disk_base` / `runner_healthy_3disk_verbose` (1472, 1549), `fs_3disk` / `fs_1disk` (1631, 1642), `pool_empty` (3450), `cfg_present_not_luks` (3462), `is_luks_raw` (3470), `luks_dump_text_raw` (3479), `report_with_alerts` (3788), `disk_report_named` (3808). Drop the now-unused imports in `mod tests`: `crate::probe::Filesystem` (introduced for the local `MockFs` impl), and any other now-orphan use lines surfaced by `cargo check --tests`. Remove `#[allow(dead_code)]` annotations on `test_fixtures::status` items now that every helper has a consumer. The migrated tests **keep** calling the prefixed forms (`status_mp`, `status_btrfs_*`, ...) AND the aliased `status_err_raw`; cleanup does NOT rename them back to bare names and does NOT remove the `use crate::test_fixtures::err_raw as status_err_raw;` line. | `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references and no `unused_imports` / `dead_code` warnings. `cargo build --manifest-path cli/Cargo.toml --tests` clean. `just test-rust` full suite green. |

### Sample migration (sub-commit 2, `status.rs:3196` -- cmd_status_healthy_ok)

Before (today's body, status.rs:3197-3204):

```rust
#[test]
fn cmd_status_healthy_ok() {
    let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
    let fs = fs_3disk();
    let config = config_3disk();
    let (_tmp, paths) = test_paths();
    let result = cmd_status(&runner, &fs, &config, false, &paths);
    assert!(result.is_ok());
}
```

After (sub-commit 2; per-test imports added at the top of `mod tests`, helper calls swap to the prefixed names):

```rust
// Added by sub-commit 2 (consolidated at the top of `mod tests` -- one
// `use` block grows across sub-commits 2-5, then is left in place by
// sub-commit 6):
use crate::test_fixtures::{
    isolated_paths, mock_ok, status_config, status_fs_three_disk,
    status_runner_healthy_3disk_base, status_runner_healthy_3disk_verbose,
};
use crate::test_fixtures::err_raw as status_err_raw;

#[test]
fn cmd_status_healthy_ok() {
    let runner = status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base());
    let fs = status_fs_three_disk();
    let config = status_config();
    let (_tmp, paths) = isolated_paths();
    let result = cmd_status(&runner, &fs, &config, false, &paths);
    assert!(result.is_ok());
}
```

Note the `cmd_status` argument order: `(&runner, &fs, &config, json: bool, &paths)` -- five args, single `bool` before `&paths` (status.rs:421-426).

The migration's per-test diff is exactly the bare-name -> `status_`-prefixed-name swap, plus a single growing `use` block at the test-mod header. Crucially, the local helpers `runner_healthy_3disk_base` (1472), `runner_healthy_3disk_verbose` (1549), `fs_3disk` (1631), `config_3disk` (1454), `test_paths` (1448) **stay in place** through sub-commits 2-5 because unmigrated tests still call them by their bare names; the prefixed `use` imports do not collide with the same-named locals because they are differently-named symbols (`status_runner_healthy_3disk_base` vs `runner_healthy_3disk_base`). Sub-commit 6 deletes the locals once every test has been migrated to the prefixed forms. The test body's structural integrity (one runner expression + one fs + one config + one paths tuple + one `cmd_status` invocation) is preserved.

## Critical files to modify

- `/Users/dan/Code/braid/cli/src/test_fixtures/status.rs` -- NEW. Items per §A, including the private `MockFs` struct + `Filesystem` impl with strict `read_to_string` semantics.
- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- add `mod status;` (private) and `#[allow(unused_imports)] pub(crate) use status::{...}` facade re-exports for the items the test mod consumes. The `unused_imports` allow follows the existing pattern at lines 73-103 -- it is required because consumers land in later sub-commits (2-5) and `cargo check --tests` would otherwise fail on the unconsumed re-exports during the staggered rollout. Update the module-level doc-comment at lines 1-62 to mention the new scope (one bullet, mirroring the `enroll_key_file` bullet at 37-51).
- `/Users/dan/Code/braid/cli/src/status.rs` -- delete the inline scaffolding listed in sub-commit 6 (lines 1130-1644 across two ranges plus the data-builder ranges at 3450-3486 and 3788-3819) and replace local references with `use crate::test_fixtures::{...}` facade imports per the table. Drop the `use crate::probe::Filesystem;` import in `mod tests` (1124-1128 region) once the local `MockFs` impl is gone, and drop any other now-orphaned use lines `cargo check --tests` surfaces.

**No `cli/src/test_fixtures/shared.rs` change.** The plan does not add `MockFs::with_mountinfo` or any other helper to shared. The new module ships its own status-scoped `MockFs` (private) so the strict `read_to_string` -> `NotFound` contract is preserved without altering shared's surface.

No production source changes outside the `mod tests` block. No other `test_fixtures/*.rs` changes (the existing scopes are untouched; status reuses only `mock_ok`, `err_raw` aliased as `status_err_raw`, and `isolated_paths` from the existing facade).

## Existing functions / utilities reused

- `crate::probe::Filesystem` trait -- the status-scoped `MockFs` struct in the new module implements this directly. Imported at the top of `cli/src/test_fixtures/status.rs` so the impl block can name it. Not exposed to consumers (helper functions return `impl Filesystem`).
- `shared::mock_ok(cmd, stdout)` (`test_fixtures/shared.rs:23`, re-exported at `test_fixtures.rs:106`) -- byte-identical to status's local `ok_raw`. Migrated tests `use crate::test_fixtures::mock_ok;` and call `mock_ok(...)`. No new status wrapper.
- `mount::err_raw(cmd, exit_code, stderr)` (`test_fixtures/mount.rs:81`, re-exported at `test_fixtures.rs:90`) -- byte-identical to status's local `err_raw`. Migrated tests **import via alias**: `use crate::test_fixtures::err_raw as status_err_raw;` and rewrite call sites to `status_err_raw(...)`. No new status wrapper. No new facade re-export. The alias stays after sub-commit 6 deletes the local; do not "simplify" it back.
- `doctor::isolated_paths()` (`test_fixtures/doctor.rs:26`, re-exported at `test_fixtures.rs:78`) -- byte-identical to status's local `test_paths`: returns `(TempDir, StatePaths)` over `StatePaths::custom(dir.path().to_owned())`. Migrated tests `use crate::test_fixtures::isolated_paths;` and call `isolated_paths()`.
- `cmd::MockRunner::with_output` / `with_output_stdin` (`cmd.rs:988`, `1004`) -- the canonical chaining surface; `status_runner_healthy_3disk_*` are flat compositions over `with_output` only.
- `cmd::MockRunner::with_handler` (`cmd.rs:1021`) -- exists, but **deliberately not used** by the status fixture. The user-supplied constraint is "use `with_handler` only where it clarifies repeated topology setup without masking request-order or missing-mock contracts" -- and the load-bearing missing-mock test at 3498 (`build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown`) plus the empty-`MockRunner` use across `MockRunner::default()`-only call sites mean a broad handler would silently flip results. Reserved for per-test override at the call site if a future status test ever needs cross-cutting field-based dispatch.

## Out of scope for this plan

- Touching `cli/src/status.rs` production code (lines 1-1121). This is a pure test-side refactor.
- Migrating other command modules. Status is the next migration target; siblings come in follow-up plans.
- Building a `StatusPool` / `StatusTopology` handler installer or `StatusParamsBuilder` (rejected in §A; no params struct, read-only, load-bearing missing-mock contracts).
- Promoting `status_disk_report_named` / `status_report_with_alerts` / `status_report_with_scrub` to `shared`. Each has only the in-tree status test mod as a consumer today. If `doctor` or another scope later grows alert-rendering coverage that needs them, promote then.
- Promoting `status_btrfs_show_*` / `status_btrfs_df_*` / `status_btrfs_usage_raw` / `status_btrfs_device_usage_raw_*` / `status_btrfs_scrub_*` / `status_btrfs_device_stats_3disk` / `status_lsblk_field_ok` / `status_cryptsetup_status_active` / `status_cryptsetup_uuid_ok` to `shared`. Other scopes either inline equivalent strings or have already promoted their own (e.g. `remove::valid_three_disk_df_json` lives at `test_fixtures/remove.rs:269`). Cross-scope unification is a separate, opt-in cleanup -- not this plan.
- Stripping the `status_` prefix from helpers whose names don't directly collide today. The prefix is applied uniformly across the new module's exports for the same two reasons enroll's plan applies it uniformly: (a) it removes the staged-migration duplicate-definition hazard for every helper, even ones whose name happens not to collide at the facade today; (b) it gives the scope a recognisable shared shape so a `grep status_` walks the entire fixture surface. Selective de-prefixing would invite the kind of "is this load-bearing or not?" question this plan exists to settle.
- Adding any setter or constructor to `shared::MockFs`. The status-scoped `MockFs` in the new module preserves status's strict `NotFound`-for-non-mountinfo contract without touching shared. If a future migration wants a parameterised mountinfo body on `shared::MockFs`, it can add the setter then -- not as a side effect of this plan.
- Promoting the status-scoped `MockFs` (or its constituent fields) to `shared`. It is intentionally narrower than `shared::MockFs` (no `exclusive_operation` resolution) and the strict semantics are status-specific. Cross-scope unification only makes sense if another scope wants the same strict contract; today none do.
- Adding a new `cmd.rs` regression test. The migration introduces no new dispatch path -- only `with_output` chains -- so the existing `MockRunner` contract tests remain sufficient.
- Migrating the 6 `estimate_pool_capacity_*` tests through fixture imports. They use no helpers today and gain nothing from the migration.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | The status-scoped `MockFs`'s `Filesystem` impl drifts from status's local one over time -- e.g. a future change to the trait adds a method, or the strict `NotFound`-for-non-mountinfo contract is silently relaxed during a refactor -- and a test starts passing for the wrong reason because a previously-NotFound read now succeeds. | The new module's `MockFs` impl is byte-copied from status.rs:1164-1183 in sub-commit 1 (`exists` -> `paths.contains(...)`, `is_block_device` -> false, `read_to_string` -> `NotFound` for everything except `/proc/self/mountinfo`, `list_dir` -> empty vec). Sub-commit 1's verification includes `cargo check --tests`, which surfaces any new trait method that is added in the future. The verification grep cited in Verification (which now includes `cli/src/mount_check.rs`) re-confirms the call surface from the status code path is still confined to `read_to_string("/proc/self/mountinfo")` + `fs.exists` + (rare) `is_block_device`. If a future change adds a code path that reads, say, `/sys/.../some_file`, that path will return `NotFound` from the status `MockFs` and the affected test will surface a loud error -- the strict semantics are the load-bearing guard. |
| 2 | The `status_fs_ext4` wrapper accidentally emits a btrfs mountinfo body, silently flipping `status_not_btrfs_maps_to_not_mounted` (3165) into a "btrfs mounted" path. | `status_fs_ext4` is explicitly defined in §A and emits the exact ext4 mountinfo body status's local `MockFs::ext4` does today (`36 35 0:32 / /mnt/storage rw shared:1 - ext4 /dev/sda1 rw\n`). Sub-commit 2 migrates `status_not_btrfs_maps_to_not_mounted` early specifically to validate the wrapper produces the right behavior; if the test starts asserting btrfs-mount semantics, the wrapper is wrong. The status `MockFs` is private to the new module, so there is no way for an external test to construct a "wrong" variant -- the only way to get an ext4 mount is through `status_fs_ext4`. |
| 3 | Promoting `status_runner_healthy_3disk_base` / `_verbose` masks a future regression where a new probe is added to the status code path that the runner doesn't seed -- leading to `MissingMock` failures in the integration tests that consume the runner. | The promoted helpers preserve the exact set of seeded `with_output` calls from `status.rs:1472-1629` verbatim. They do NOT use `with_handler`. If a future change adds a probe (e.g. a fresh `LsblkField::Wwn`), the `cmd_status_healthy_*` and `status_json_verbose_disks` tests that consume the runner will surface `MissingMock` and the test fails loudly with the offending probe in the error. Add a one-paragraph doc comment on the runner composers listing what they DO seed and noting that a `MissingMock` from a consumer means the runner is out of date with the production probe set. |
| 4 | The `build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown` test (3498) expects `DiskStatus::Unknown` from a `MockRunner::default()` (no outputs). If a future "convenience" wrapper auto-resolves `CryptsetupIsLuks` / `CryptsetupLuksDumpText`, the assertion silently flips to `LuksHeaderUnreadable` and the test passes for the wrong reason. | The new module ships **no** broad handler installer (Context's "intentional omissions"). `status_pool_empty` and `status_cfg_present_not_luks` are pure value builders -- they touch neither the runner nor the fs. Sub-commit 4's per-test verification includes a manual diff of `runner.requests()` for the probe-failed test before and after migration: the empty `Vec<CmdRequest>` must round-trip. |
| 5 | The eight JSON-envelope tests pin exact field omission / inclusion. If the migration accidentally introduces a topology runner (or a data builder default) that adds a `Some(ScrubReport::Never)` to a previously-`None` `last_scrub`, the envelope shape changes silently. | Sub-commit 3 is import-only -- the leaf factories are byte-identical to today's locals (`btrfs_show_3disk` etc. emit the same `RawCommandOutput` bytes; `status_disk_report_named` builds the same default `DiskReport` shape). No `with_handler` is introduced. `MockRunner::run` always logs (`cmd.rs:1172-1175`) regardless of how dispatch resolves. The eight JSON tests run individually post-migration and the resulting `serde_json::Value` is `assert_eq!`'d against the pre-migration value. |
| 6 | Migration accidentally drops a `// Intent / Why it exists / Scenario` preamble during a test rewrite -- including the dense ones at 3311-3340 (missing_devids), 3488-3497 / 3509-3518 / 3539-3547 / 3576-3589 (PresentNotLuks classification), 3720-3729 (verbose unknown), 3910-3917 / 3939-3947 (corrupt pool.json), 3966-3980 (mapper conflict), 4113-4130 (devid pairing). | AGENTS.md's "Test Conventions" section makes the preamble part of the test contract. Verification (per sub-commit) includes `git log -p cli/src/status.rs` -- the diff for each migrated test must show body changes only (the `use ...` import line and the local-helper -> `status_`-prefixed-name swaps), with preamble lines unchanged. |
| 7 | A reviewer reads the new `status_fs_three_disk()` thin wrapper and decides to "simplify" by inlining `status_fs_mounted(&[...])` calls everywhere, then later breaks the convention. | The wrapper exists for ergonomics (caller passes nothing; the canonical 3-disk path set is encapsulated) and as a single point of change if status tests ever need a different shared mock variant. Add a one-line `pub(crate) fn` doc explaining both. Mirrors the same wrapper rationale at `mount.rs:42-50`. |
| 8 | A reviewer reads the `status_*` prefix on every helper (and the `err_raw as status_err_raw` import alias) and decides to "simplify" the names back to bare forms because mount and doctor mostly ship unprefixed names. | The prefix and the alias are load-bearing for two distinct reasons captured at the top of the new module's doc comment AND in a one-line comment alongside the `use crate::test_fixtures::err_raw as status_err_raw;` import line at every call site that needs it: (a) facade collisions with `mount::err_raw` / `mount::ok_raw` / the type-name `MockFs` from `shared`; (b) the staged migration's same-module `use` + local `fn` duplicate-definition error (which ALSO applies to the `err_raw` reuse, since the local helper is named `err_raw` -- not `mock_ok` / `isolated_paths`, whose locals are differently named and don't need the alias). Any de-prefixing or de-aliasing breaks one or both. The module-level doc comment quotes these constraints so a future reviewer doesn't try a sweep without re-deriving the rationale. |
| 9 | The two corrupt-pool.json tests (3909, 3949) and the alert-latch test (4206) write intentionally-corrupt bytes inline via `std::fs::write(paths.pool_json(), ...)` / `std::fs::write(paths.alert_latch_json(), ...)`. A future contributor reads them, decides "this looks like boilerplate", and writes a fixture wrapper. | The inline `std::fs::write` is the **visible** "this is the byte sequence that triggers the regression" intent, and the bytes vary across the three tests (truncated JSON / invalid JSON / one-token-then-truncated JSON). A wrapper would obscure that visibility. Document the no-wrapper decision in the new module's intentional-omissions list (§A) so the rationale is visible at the obvious place to look. |
| 10 | The integration-style scrub-parsing tests (2567, 2588, 2613, 2637, 2661) feed a `MockRunner` with `status_btrfs_scrub_*` outputs and call `get_scrub_report`. If the new factories' bytes diverge from today's (typo, wrong timestamp, missing newline), the resulting `ScrubReport` variants flip and the assertions fail. | The five scrub factories in §A are byte-identical to status's locals at 1374-1431. Sub-commit 5's verification includes per-test runs for each of the five plus the four pure JSON-serialization tests that consume them indirectly via `status_report_with_scrub` -- a divergent factory shows up in either form. |
| 11 | The `disk_report_pairs_stats_by_devid_when_path_differs` test (4131) gets accidentally migrated despite having no fixture-eligible setup -- a contributor sees it in the highest-risk family table and rewrites its body to use `status_*` helpers it does not need, introducing churn and risking a regression in the dense intent preamble (4113-4130). | The test is explicitly marked **no-touch** in sub-commit 2's migration row: it uses `MockRunner::default()` plus literal `PoolState` (status.rs:4136-4149), `ConfigDisk` (4150-4158), and `BtrfsDeviceStatsOutput` (4161-4171) values, and calls `build_disk_reports(&runner, &config_disks, &pool, &stats)` directly -- there is no runner / fs / config / paths setup to migrate. Sub-commit 2 runs the test for regression coverage but leaves its body untouched. The test's intent preamble (4113-4130) round-trips byte-for-byte for the trivial reason that the file region is unchanged. |
| 12 | Status's local `config_3disk` and `config_1disk` are byte-identical (both call `Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()`); the migration collapses them into a single `status_config()`. A future single-disk vs 3-disk semantic divergence (e.g. a `Config::with_disk_count` setter) would land confused if the names change. | Status's `Config` today carries no disk-count field -- it's a plain `MountPoint` wrapper. The collapse is correct as of this plan. If a future change adds a disk-count field or other topology-coupled state, split `status_config_one_disk` / `status_config_three_disk` then. |

## Verification

End-to-end gate: `just test-rust` is green at every sub-commit boundary. `test-rust` (`Justfile:104`) runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard` as a fixed command. Filtered runs go through `cargo test` directly.

**Pre-sub-commit-1 verification (one-time):**

```
grep -nE "fs\.(read_to_string|is_block_device|list_dir)" cli/src/status.rs cli/src/mount_check.rs cli/src/probe.rs
```

Confirms which `Filesystem` trait methods are called from the status call graph. The status code path reaches `/proc/self/mountinfo` through `mount_check::fstype_at_mount_via_fs` (`cli/src/mount_check.rs:172-178`), which is the authoritative parser status uses to distinguish mounted-btrfs / not-mounted / mounted-ext4. The grep includes `cli/src/mount_check.rs` because the `*.rs` glob would otherwise miss it (`cli/src/probe` is a single file, not a directory). Any `read_to_string` calls outside `/proc/self/mountinfo` (the only path the status `MockFs` resolves) would mean a code path expects a different mock surface and the strict `NotFound` semantics need adjustment. Today's call sites are confined to `mount_check::fstype_at_mount_via_fs` / `mount_check::is_btrfs_mounted` / `mount_check::mount_entry_at_via_fs` (all reading `/proc/self/mountinfo`) plus `fs.exists` for path-existence checks; both round-trip through the new module's `MockFs`.

**Per sub-commit:**

- **Sub-commit 1** (scaffolding the new module + private `MockFs` struct + `Filesystem` impl; no `shared.rs` change): `cargo check --manifest-path cli/Cargo.toml --tests` clean (no `unused_imports` / `dead_code` errors -- the `#[allow(...)]` annotations cover the staggered consumer rollout). Then `just test-rust` green.
- **Sub-commit 2** (highest-risk family, 11 migrated tests + 1 no-touch coverage run, import-only): per-test runs --
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_not_mounted_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_healthy_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_healthy_json_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_degraded_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_single_disk_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::build_status_missing_devids_unions_btrfs_missing_and_null_underlying`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_surfaces_mapper_conflict`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_corrupt_membership_returns_error`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::cmd_status_unmounted_corrupt_membership_returns_ok`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_not_btrfs_maps_to_not_mounted`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::resolve_alert_state_surfaces_corrupt_latch_as_computation_error`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::disk_report_pairs_stats_by_devid_when_path_differs` -- **no-touch coverage run**: the test body is unchanged in this sub-commit (it uses `MockRunner::default()` + literal struct values; nothing to migrate). The run confirms the migration's fixture additions did not break it indirectly.

  Then `cargo test --manifest-path cli/Cargo.toml --lib status::tests`. Then `just test-rust`. The corrupt-pool.json tests must continue to surface `StatusError::Membership(Corrupt(..))` (mounted) and `Ok(())` (offline). The mapper-conflict test must continue to surface `StatusError::Probe(MapperConflict)`. The devid-pairing test (no-touch) must continue to assert `errors.read == 5` (status.rs:4181-4184). If any of these flips, the leaf factory in the new module emits a different `(CmdRequest, ...)` shape than the local one and must be corrected before the sub-commit lands.
- **Sub-commit 3** (JSON envelope + human output + verbose disk rendering, ~18 tests, import-only): per-family --
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_json` (matches all 8 JSON envelope tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::not_mounted_status_envelope_is_minimal`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_human` (5 human output tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_verbose` (4 verbose tests at 2303-2517 plus status_verbose_unknown_disk in sub-4)

  Then full `status::tests`, then `just test-rust`. The `disks` always-array invariant (1946-2030), the offline envelope's exactly-four-keys invariant (1719-1740), the singular/plural missing-device wording switch (2215, 2268), and the LUKS-header label wording (2407, 2469) must round-trip.
- **Sub-commit 4** (PresentNotLuks classification + compact + verbose-unknown, ~7 tests): per-test --
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::build_disk_reports_present_not_luks` (5)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::build_disk_reports_skips_unpooled_row_when_mapper_in_pool_for_present_not_luks`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_compact_missing_disk`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_verbose_unknown_disk`

  Then full `status::tests`, then `just test-rust`. The probe-failed test (3498) and the inconsistent-falls-back-to-unknown test (3591) must both continue to surface `DiskStatus::Unknown`. The unreadable test (3519) must continue to surface `DiskStatus::LuksHeaderUnreadable`; the damaged test (3548) must continue to surface `DiskStatus::LuksHeaderDamaged`.
- **Sub-commit 5** (long tail bulk migration, ~32 tests): per-family --
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_scrub` (5 integration parsing tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::scrub_report_json` (6 pure JSON serialization tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::human_scrub` (4 pure human rendering tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::balance_report` (4 parsing tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::emit_paused_balance_warning` (2 paused-warning emission tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::balance_human` (3 balance-human tests including idle_no_line)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::estimate_pool_capacity` (6 pure capacity tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::get_capacity_raid1_used_is_logical`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_df_failure_fatal`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_usage_failure_fatal`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::status_device_stats_failure_fatal`
  - `cargo test --manifest-path cli/Cargo.toml --lib status::tests::alert_` (3 alert-rendering tests)

  Then full `status::tests`, then `just test-rust`. The five integration scrub-parse tests must produce the same `ScrubReport` variants as today's locals; the four pure scrub-human tests must produce the same string snippets ("(no errors)", "(3 errors)", "cancelled (will resume)", "interrupted"); the six `estimate_pool_capacity_*` tests must produce the same `u64` outputs (0, 4TB, 4TB, 6TB, 6TB, 7TB).
- **Sub-commit 6** (cleanup): `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references and no `unused_imports` / `dead_code` warnings. `cargo build --manifest-path cli/Cargo.toml --tests` clean. `just test-rust` full suite green. The `#[allow(dead_code)]` annotations on `test_fixtures::status` items are removed and `cargo build` still clean.

**Behavior-preservation check (mechanical, all sub-commits):**

- Every `// Intent / Why it exists / Scenario` preamble round-trips byte-for-byte. `git log -p cli/src/status.rs` per sub-commit -- diff for each migrated test shows body changes only.
- Every `assert!(...)` / `assert_eq!(...)` / `assert!(matches!(...))` body is unchanged across the migration -- the migration touches setup code (runner, fs, config, paths, membership, raw command output factories, `StatusReport` data builders) only.
- Every `serde_json::to_value(&report)` round-trip preserves the same `Value` structure (key set, type per key, omission of `None` fields).
- Every `format_human_status(&report, verbose)` (or equivalent renderer) produces the same `String` output -- substring assertions pin the user-facing wording.
- The corrupt-pool.json regression tests' inline `std::fs::write(paths.pool_json(), <bytes>)` calls round-trip with the same byte sequences (status.rs:3922, 3953).
- The alert-latch regression test's inline `std::fs::write(paths.alert_latch_json(), <bytes>)` call round-trips with the same byte sequence (status.rs:4210).
- Every `cmd_status(...)` invocation matches the production signature `(&runner, &fs, &config, json: bool, &paths)` -- one bool before `&paths`, not two.
- Every `MockRunner::default()` (empty) call site is preserved (no broad handler is introduced that would auto-resolve probes).

No new VM tests, no parser-fixture refresh, no production behavior change. The existing test suite IS the verification.

## Branch and commit shape

Work on a feature branch (e.g. `refactor-status-test-fixtures`). Each numbered sub-commit above is one git commit. PR opens once sub-commit 6 lands. Reviewer can walk the branch commit-by-commit; each commit is independently green.

Conventional Commits-style messages (lowercase first word per AGENTS.md):

- `refactor(test): add status scope test fixture module` (sub-commit 1)
- `refactor(status): migrate cmd-status integration and structural-integrity tests to shared fixtures` (sub-commit 2)
- `refactor(status): migrate json envelope, human output, and verbose-disk tests to shared fixtures` (sub-commit 3)
- `refactor(status): migrate present-not-luks classification and compact-drive tests to shared fixtures` (sub-commit 4)
- `refactor(status): migrate scrub, balance, capacity, and alert-rendering tests to shared fixtures` (sub-commit 5)
- `chore(status): drop migrated locals from status tests module` (sub-commit 6)
