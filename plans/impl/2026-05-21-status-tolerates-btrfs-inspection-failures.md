# Plan: make `braid status` tolerant of df / usage / device-usage / device-stats failures

## Context

`braid status` is one of two read-only diagnostics in the project (`status` and `doctor`). `docs/principles.md:69` explicitly carves these out as "Read-only diagnostics ... so operators retain a working diagnostic surface during contention" -- they bypass the pool lock for the same reason.

Today `cli/src/status.rs:415-447` violates that role asymmetrically:

- `fetch_df` (line 415), `get_capacity` (line 417), and `get_device_stats` (line 447) propagate `?`. Any non-zero exit or parse error from `btrfs filesystem df`, `btrfs filesystem usage`, `btrfs device usage`, or `btrfs device stats` makes the whole command exit 1 via `main.rs:651-661`.
- `get_scrub_report` (line 663) and `get_balance_report` (line 723) already swallow the same shape of failure into `ScrubReport::Unknown` / `BalanceReport::Unknown`, and `format_status_human` renders both as "unknown" inline.
- The sibling diagnostic `doctor` already treats `BtrfsFilesystemDfJson` failure as best-effort (`doctor.rs:570-585`), surfacing it as a `CheckResult::warn` ("could not inspect ... profiles: {e}").

The asymmetry developed by accretion: the three `*_fatal` helper tests at `status.rs:2647`, `2659`, `2765` were added in the initial phase-5 status implementation (`9a14b6f`); the tolerant `Unknown` variants for scrub and balance were added later by separate feature commits and never retrofitted to the original four gather points.

Outcome: when btrfs transiently returns a non-zero exit on any one of df / usage / device-usage / device-stats (e.g. during a `replace` swap, kernel busy state, or parser drift), the operator's first-resort diagnostic dies instead of rendering whatever it could read. This plan aligns the four remaining commands with the existing tolerant pattern.

The `StatusReport` schema is already in place -- `profile`, `capacity`, `allocation`, and per-disk `errors` are all `Option<...>` with `#[serde(skip_serializing_if = "Option::is_none")]` (`status.rs:57-65`, `189-190`). `CapacityReport.total_bytes` is also already `Option<u64>` (`status.rs:93`). No struct changes are needed.

## Critical files

- `cli/src/status.rs` -- the only file with code changes.

## Changes

### 1. `cli/src/status.rs` -- make the four gathers tolerant in `build_status`

Around `cli/src/status.rs:415-447`, replace the `?` propagation with `.ok()` / pattern-match degradation, and push an advisory string when a gather fails. Each tolerant boundary tolerates BOTH the command-failure path (`runner.run(...)` returns `Err`) AND the parse-failure path (the parser returns `Err`) -- matching the existing precedent in `get_scrub_report` (`status.rs:663-712`) which uses one `match` on `runner.run` and a separate `match` on `parse_btrfs_scrub_status`. Each section maps to specific report fields the way `summarize_df` and `build_disk_reports` already join the data:

| Failed command | StatusReport effect | Advisory pushed |
|---|---|---|
| `BtrfsFilesystemDfJson` (`fetch_df`) | `profile=None`, `allocation=None`, `capacity=None` (because used_bytes is derived from df) | `"btrfs filesystem df failed -- pool capacity, allocation, and profile unavailable"` |
| `BtrfsFilesystemUsageRaw` (inside `get_capacity`) | `capacity=None` | `"btrfs filesystem usage failed -- pool capacity unavailable"` |
| `BtrfsDeviceUsageRaw` (only when `missing_count == 0`) | `capacity.total_bytes=None`, rest of `capacity` intact | `"btrfs device usage failed -- pool total capacity unavailable"` |
| `BtrfsDeviceStatsJson` (`get_device_stats`) | per-disk `errors=None` for every disk (the existing `.find()` join at `status.rs:834-844` produces this automatically when `device_stats.devices` is empty) | `"btrfs device stats failed -- per-disk error counts unavailable"` |

Concretely:

- `fetch_df` and `get_device_stats` stay as `Result`-returning helpers (their signatures don't need to change). `build_status` calls them via `match` and on `Err` records an advisory plus leaves the relevant fields as `None` / empty. Because the helpers internally `?` both command failures and parse failures into the same `StatusError`, a single `.ok()` / `match` in `build_status` covers both failure modes.
- Split `BtrfsDeviceUsageRaw` out of `get_capacity` into a new helper `get_total_bytes(runner, mount_point) -> Result<u64, StatusError>`. `build_status` calls it only when `missing_count == 0`, matches on the result, pushes the device-usage advisory on `Err`, and passes `total_bytes: Option<u64>` into a refactored `get_capacity(runner, mount_point, df, total_bytes)`. This isolates the device-usage failure surface so it gets its own advisory instead of being silently degraded to a missing "Total:" line. (Rationale: silent `total_bytes=None` only matches expectations on a degraded pool because the `Status: DEGRADED` line explains the missing Total; on a healthy pool there is no other indicator that the command failed, which is the asymmetry this whole plan is fixing.) Per `AGENTS.md:137-145` (doc-comment requirement for new top-level Rust CLI items), `get_total_bytes` gets a one-to-three-line `///` doc comment justifying why it exists as a separate helper -- e.g. `/// Separate from get_capacity so device-usage failures can advisory-degrade total_bytes=None independently of the df / usage-derived used/free bytes.`
- `get_device_stats` failure: in `build_status`, fall back to `BtrfsDeviceStatsOutput { devices: vec![] }` so `build_disk_reports` keeps working and every disk's `errors` field becomes `None` via the existing devid-keyed `.find()` join.
- Advisories are pushed onto the existing `advisories: Vec<String>` (the same channel `not_btrfs_surfaces_fstype_advisory` and `header_backup_advisories` already use). They render at the top of human output via `format_status_human:983-985` and serialize to JSON via the existing `Vec<String>` field.

No changes to `format_status_human` are needed: empirically confirmed by the exploration trace that `profile=None` renders nothing, `capacity=None` skips the whole "Capacity:" block, `allocation=None` skips the whole "Allocation:" block, and per-disk `errors=None` on a `Present` disk currently renders no errors line. Operators learn about the partial failure from the top-of-output `warning: ...` advisories, which is the precedent already set for the foreign-fstype case and matches `doctor`'s "could not inspect ..." pattern.

### 2. Tests -- replace the three `*_fatal` helper tests with behavioral pipeline tests

Delete `cli/src/status.rs:2647-2656` (`status_df_failure_fatal`), `2659-2669` (`status_usage_failure_fatal`), and `2765-2774` (`status_device_stats_failure_fatal`). These pin obsolete contracts -- after the fix the helpers no longer fail-propagate, so the tests are meaningless.

Add eight new behavioral tests against the full `build_status` pipeline -- one **command-failure** test and one **parse-failure** test per tolerant boundary, since `braid status`'s tolerance contract covers both runner errors (non-zero exit) and parser drift. Without the parse-failure variant, a regressing implementation that only `.ok()`s the runner call but still propagates parse errors with `?` would pass every command-failure test.

Use the existing override pattern from `build_status_missing_devids_unions_btrfs_missing_and_null_underlying` (`status.rs:3002`): start from `status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base())` (`test_fixtures/status.rs:425, 502`) -- the *verbose* variant is required so `mounted_extras` is populated for human-format assertions -- and override one command. Command-failure tests use `status_err_raw("btrfs ...", 1, "msg")` (`test_fixtures/mount.rs:80`). Parse-failure tests use `mock_ok("btrfs ...", "garbage not parseable")` (`test_fixtures/shared.rs:73`) -- the command succeeds (exit 0) but the stdout fails the parser. Each test must contain a **degraded-section block** AND a **retained-section block** (the latter prevents a "drop everything on any one failure" regression from passing):

**Degraded-section assertions (one boundary, one failure mode):**

1. Build status via `build_status(...)`, assert it returns `Ok(...)` (not `Err(...)`).
2. Assert the failed boundary's report fields are `None` / empty per the section-1 table.
3. Assert `built.report.advisories` contains the expected substring (per the section-1 table) -- shape mirrors `doctor.rs:3300-3310`'s "could not inspect ..." substring assertion.
4. Render via the same shape as `cmd_status` at `status.rs:507-519`:
   ```rust
   let extras = built.mounted_extras.as_ref();
   let human = format_status_human(
       &built.report,
       extras.map(|e| e.compact_drives.as_slice()),
       extras.map(|e| e.human_details.as_slice()),
       extras.map(|e| &e.devid_names),
   );
   ```
   Assert the corresponding section is absent and the `warning: ...` line is present -- shape mirrors `balance_human_unknown` (`status.rs:2588`).

**Retained-section assertions (every test must include all of these):**

5. `built.report.status == StatusCode::Intact` (no failure escalates the pool's status code).
6. `built.report.total_devices == Some(3)`, `present_count == Some(3)`, `missing_count == Some(0)`.
7. `built.report.disks.len() == 3` -- the disk list survives every failure mode (per the existing healthy-3disk fixture).
8. Per-boundary retained-field matrix (each test asserts the *other* sections are still populated):
    - **df failure** (`profile`/`allocation`/`capacity` gone): assert per-disk `errors` still `Some(...)` on all three disks, `last_scrub` is `Some(...)`, `balance` is `Some(...)`.
    - **usage failure** (`capacity` gone): assert `profile == Some(...)`, `allocation == Some(...)` (non-empty), per-disk `errors` still `Some(...)`, scrub/balance still present.
    - **device-usage failure** (only `capacity.total_bytes` gone): assert `capacity == Some(CapacityReport { total_bytes: None, used_bytes: <healthy>, free_bytes: <healthy> })`, `profile`/`allocation`/`errors` all still populated.
    - **device-stats failure** (per-disk `errors` gone): assert `capacity`/`profile`/`allocation` all populated, every `disk.errors == None`, but `disk.name`/`mapper`/`by_id`/`luks_uuid`/`devid`/`underlying`/`status` are unchanged.
9. Render the human format and assert the retained sections are still in the output: the `Drives:` compact section (always-on for mounted pools), the per-disk verbose section (one block per disk from `human_details`), and any sections not affected by this boundary's failure (e.g. `Allocation:` is still present in a usage-failure test, `Last scrub:` is present in all four). The advisory `warning:` line and the absent section together pin the failure surface; the retained-section assertions pin everything else.

Proposed names (matching the existing tolerant-test naming convention `status_scrub_failure_tolerant` at `status.rs:2263`):

- `build_status_df_cmd_failure_tolerant` / `build_status_df_parse_failure_tolerant`
- `build_status_usage_cmd_failure_tolerant` / `build_status_usage_parse_failure_tolerant`
- `build_status_device_usage_cmd_failure_tolerant` / `build_status_device_usage_parse_failure_tolerant`
- `build_status_device_stats_cmd_failure_tolerant` / `build_status_device_stats_parse_failure_tolerant`

The device-usage pair specifically asserts: `built.report.capacity` is `Some(CapacityReport { total_bytes: None, .. })` (used / free still populated from the successful df / usage), the `warning: btrfs device usage failed -- pool total capacity unavailable` advisory line is present, and the human-format "Total:" line is absent while "Used:" and "Free:" remain.

**Test preamble (required by `docs/testing.md:11-22` and `AGENTS.md`):** every new test must begin with a contiguous `//` line-comment preamble:

```rust
// Intent: ...
// Why it exists: ...
// Scenario: ...
#[test]
fn build_status_df_cmd_failure_tolerant() { ... }
```

This applies uniformly to all eight new pipeline tests. No exceptions; the linter / test-suite pattern is enforced via convention review, not tooling, so it must be called out in the plan.

## Existing utilities / patterns to reuse

- `MockRunner::with_output` (`cli/src/cmd.rs:1367-1371`) -- HashMap-backed override; second call for the same `CmdRequest` overrides the first. This is what lets the new tests start from `status_runner_healthy_3disk_base()` and replace exactly one command's response.
- `status_err_raw("btrfs ...", 1, "msg")` (test helper, used at `status.rs:2263` and the existing three `*_fatal` tests) -- canonical error-injection factory.
- `status_runner_healthy_3disk_base()` and `status_runner_healthy_3disk_verbose()` (`test_fixtures/status.rs:425`, `502`) -- the shared healthy-pool runner.
- `status.rs:983-985`'s advisory rendering loop -- already prints every advisory as `warning: ...` at the top of human output.
- `doctor.rs:570-585` -- the sibling tolerant pattern; if any review pushes back on the advisory wording, copy doctor's `"could not inspect {label}: {e}"` shape verbatim instead.
- `BalanceReport::Unknown` rendering at `status.rs:1039-1041` and its test `balance_human_unknown` at `status.rs:2588` -- canonical "section degraded to known-unknown" precedent (this fix uses a slightly different strategy -- silent omission + top-of-report advisory -- because the affected sections are blocks, not single lines, but the spirit is the same).

## Verification

- `just test-rust` -- runs the eight new pipeline tests, the existing tolerant-scrub / tolerant-balance tests, and all other status tests. Must pass.
- `just test-vm braid-status-rust` -- the existing VM-level status canary at `tests/braid-status-rust.py`. Confirms the change does not regress the happy-path output against real `btrfs-progs` output in a live VM.
- Optional manual sanity check in a VM: stop the btrfs filesystem briefly (or run `btrfs filesystem df /tmp` against a non-btrfs path so the command exits non-zero) and confirm `braid status` still renders the rest of the report with the expected `warning:` lines.

## Out of scope

- Finer-grained partial `CapacityReport` (e.g. preserving `used_bytes` from df when `usage` fails) -- would require promoting `used_bytes` / `free_bytes` to `Option<u64>`. Not justified by any observed need; today's coarse "whole capacity is None on usage failure" matches the existing degraded-pool behavior at `status.rs:630-639`.
- Rendering "Errors: unknown (metadata unavailable)" inline for `Present + None` disks (the catch-all at `status.rs:1173`). Keeping that branch silent preserves minimal change; the top-of-output advisory provides the visibility. Easy follow-up if desired.
- Changes to `doctor` -- its handling is already correct.
- Changes to `cmd_status`'s exit code on advisory presence. Status returns 0 on a successful (possibly-partial) report; this matches the existing tolerant behavior for scrub / balance unknown.

## Implementation notes

- The device-usage parse-failure test uses a malformed device stanza missing `Device size` because `parse_btrfs_device_usage` treats arbitrary no-device text as an empty successful output, so plain garbage would not exercise parse-error degradation.
