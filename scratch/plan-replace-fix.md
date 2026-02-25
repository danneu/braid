# Migration Plan: Enable `braid replace --old` for Live Disk Targets

## Summary
Expand `braid replace` from "dead/missing old disk only" to "live or missing old disk," while preserving add-first ordering and strict safety checks.

This plan is now committed to a specific implementation pivot: **`replace` must reuse the same present-device eviction logic used by `remove`**. Live target eviction behavior (including any RAID profile conversion required before removal) must be implemented as a shared primitive, not duplicated across commands.

This plan includes coordinated updates to code, tests, and docs/decision records to keep architecture authority and behavior aligned.

## Implementor Directives (Mandatory)
1. **Mandatory pivot:** implement live-old eviction through one shared primitive used by both `remove` and `replace`.
2. Do not add bespoke live-eviction logic inside `replace`.
3. Keep checkpoint schema/version unchanged; keep `Phase::ReplaceEvictDead` name/value.
4. Shared primitive must include **post-remove LUKS close** (`cryptsetup close <old-mapper>`) as best-effort warning-only behavior.
5. Shared primitive must decide conversion need from **current live pool probe + target mapper**, not from caller-provided projected counts.
6. Keep eviction resumability in a single replace eviction phase; do not add phase complexity unless strictly required.

## Locked Product Decisions
1. `--old` validation: **pool-membership only** for live path (no config requirement for `--old`).
2. Scope: support live replace in **all topologies**, including operations that end with a single-device pool.
3. Mixed state policy: if `--old` is live and pool has any missing devices, **reject** and require cleanup first.
4. `--missing-id` behavior: if `--old` is live, `--missing-id` is **invalid** and rejected.
5. Checkpoint schema version remains unchanged for this migration.
6. `Phase::ReplaceEvictDead` name and env value are retained for compatibility with existing checkpoints.
7. **Implementation mandate (pivot):** live-device eviction semantics are centralized in one shared helper used by both `remove` and `replace`.
8. Single-device final topology safety: when projected post-eviction device count is 1, conversion from RAID1 to single profile is required before removing the live device.

## Public Interface / Behavior Changes
1. `braid replace --old <name> --new <name>` now accepts:
- live `--old` disk currently in pool, or
- missing/dead old disk (current behavior).
2. `--missing-id` remains valid only for missing/dead path.
3. New validation errors:
- live old + missing devices present: explicit reject with guidance to run `braid remove-missing` or dead-disk replace first.
- live old + `--missing-id`: explicit reject as incompatible flags.
- `--old == --new`: reject.
4. Confirmation behavior:
- default confirm phrase remains `yes`.
- if operation will end with a single-device pool (no redundancy), require explicit phrase: `replace without redundancy`.

## Implementation Plan

### 1. Replace target resolution refactor
File: `cli/src/replace.rs`

1. Introduce target classification:
- `EvictionTarget::Live { mapper: MapperName }`
- `EvictionTarget::Devid(u64)`
- `EvictionTarget::Missing`
2. Replace current early "live old => refuse" branch with resolver logic:
- If old mapper found in pool:
  - if `missing_id.is_some()`: reject.
  - if `pool.missing_count > 0`: reject (mixed state policy).
  - else choose `EvictionTarget::Live`.
- Else use existing missing-target resolution (`missing_id`, missing count rules).
3. Keep `--new` config requirement unchanged.
4. Add `old_name == new_name` validation early.

### 2. Shared present-device eviction primitive (Committed Pivot)
Files: `cli/src/remove.rs`, `cli/src/replace.rs`, `cli/src/pool.rs` (or dedicated shared module)

1. Extract present-device eviction behavior into a shared helper used by both commands.
2. Helper contract must include:
- target mapper/device path (`/dev/mapper/...`)
- mount point
- progress output
3. Helper behavior must be exactly:
- probe current pool state and determine if live removal would leave one device.
- if removal would leave one device: run profile conversion RAID1 -> single (`pool_balance_single`) before remove.
- then run `pool_remove_device` for the target mapper.
- then run `cryptsetup close` for the target mapper (best effort; warn on failure, do not fail command).
4. `remove` must switch to this helper (replace existing inline conversion/remove logic).
5. `replace` live-path must call this helper instead of introducing bespoke live remove logic.

### 3. Execution flow changes in `cmd_replace`
File: `cli/src/replace.rs`

1. Keep pre-eviction path unchanged:
- probe new disk
- optional LUKS format/open
- `btrfs device add`
- checkpoint creation at `ReplaceBalanceRaid1`
- balance RAID1
2. Change eviction phase dispatch:
- `Live`: call shared present-device eviction helper (helper owns conversion decision from live probe).
- `Devid`: existing `pool_remove_devid` path.
- `Missing`: existing `pool_remove_missing` path.
3. Keep checkpoint schema version unchanged.
4. Resume gate `secondary_target_available` semantics:
- true if live old exists in pool OR missing target exists (`pool.missing_count > 0` or `missing_id` provided).

### 4. Checkpoint/resume behavior for conversion-before-live-evict
Files: `cli/src/checkpoint.rs`, `cli/src/replace.rs`

1. Preserve `ReplaceEvictDead` phase name/value for compatibility.
2. Ensure resume remains deterministic when single-profile conversion is required before live remove.
3. Keep replace resume logic in this phase and make eviction idempotent:
- re-probe pool at eviction execution time.
- if live target mapper already absent, skip remove/close and continue.
- if target present, apply conversion/remove/close flow via shared helper.
4. If additional phase granularity is added for conversion, do not bump schema version; treat it as additive phase support while continuing to accept existing checkpoints.
5. Fail-closed behavior remains unchanged for topology/config/target drift.

### 5. Dry-run and confirmation messaging
File: `cli/src/replace.rs`

1. Update dry-run step rendering:
- live path final step: `btrfs device remove /dev/mapper/braid-<old>`.
- when projected remaining count is 1, include conversion step before remove:
  - `btrfs balance -dconvert=single -mconvert=single -f`.
- missing path remains unchanged.
2. Update confirmation text:
- remove "dead/new" wording in generic prompt.
- preserve LUKS destructive warning for new disk when relevant.
3. Add redundancy-loss confirmation gate:
- compute projected post-replace topology outcome.
- if projected final pool has one device, require `replace without redundancy`.

### 6. CLI help text and command descriptions
File: `cli/src/main.rs`

1. Update subcommand description from "Replace a dead disk with a new one" to neutral wording (live or dead).
2. Update `--old` arg help text from "Name of the dead disk to replace" to "Name of the disk to replace."

## Test Plan

### 1. Unit tests (Rust)
Files: shared eviction helper location, `cli/src/replace.rs`

Add tests for:
1. live old resolution succeeds when no missing devices.
2. live old + `--missing-id` rejects with clear error.
3. live old + pool missing devices rejects with clear error.
4. `old == new` rejects.
5. dry-run steps include mapper remove for live path.
6. dry-run steps include RAID1->single conversion when projected remaining count is 1.
7. generic confirmation text no longer claims old is dead.
8. no-redundancy confirmation trigger logic for live replace in single-device final topology.
9. shared helper calls conversion before remove when projected remaining count is 1.
10. shared helper skips conversion when projected remaining count > 1.
11. shared helper attempts `cryptsetup close` after successful remove and surfaces warning-only behavior on close failure.

### 2. Existing dead-path integration remains
Files: `tests/7-replace-failed-disk.nix`, `tests/replace-failed-disk.py`

1. Keep as regression test for degraded/missing replacement flow.
2. Update wording/comments only where behavior text implies dead-only command semantics.

### 3. New live-replace integration test
Add files:
- `tests/26-replace-live-disk.nix`
- `tests/replace-live-disk.py`
Update:
- `flake.nix` checks set to include `replace-live-disk`.

Scenarios in new VM test:
1. Build healthy RAID1 pool.
2. Run live replace `disk2 -> disk3`.
3. Assert:
- `disk2` removed from pool.
- `disk3` present.
- no missing devices introduced.
- RAID profile valid for resulting topology.
- data preserved.
4. Validate mixed-state rejection:
- simulate missing device, then attempt live replace and assert explicit failure guidance.
5. Validate `--missing-id` rejection on live path.

### 4. New single-device-final-topology integration scenario (required)
Files: include in `tests/26-replace-live-disk.nix` / `tests/replace-live-disk.py` or separate dedicated test pair

1. Start from topology where post-replace pool will end with one device.
2. Execute live replace.
3. Assert conversion path is honored and command succeeds.
4. Assert final pool has one device and expected single-profile semantics.
5. Assert no data loss.

### 5. Checkpoint resume coverage for replace
File: `tests/braid-checkpoint-opstate.py`

Add subtests:
1. existing replace interruption/resume still succeeds for dead/missing path.
2. interruption/resume succeeds for live path without conversion requirement.
3. interruption/resume succeeds for live path when conversion-before-evict is required.
4. resumed live-evict path is idempotent when target is already absent at resume time.

### 6. Test file header convention (mandatory)
All new test files must start with the required block comment describing:
1. What is being tested.
2. Why it exists and what architecture guarantee it validates.
3. Dependencies that must already hold.

## Docs / Architecture Authority Updates

### 1. User guide
File: `README.md`

1. Rename "Replace a failed drive" section to broader wording.
2. Document two supported modes:
- live swap (capacity upgrade / proactive replacement),
- dead/missing replacement.
3. Document mixed-state rejection and required cleanup sequence.
4. Document `--missing-id` as missing-path-only.
5. Clarify redundancy semantics:
- add-first ordering prevents temporary drop during migration,
- final redundancy depends on resulting device count,
- some replacements intentionally end in single-device topology and require explicit confirmation.

### 2. Decision record updates
File: `docs/decisions/intent-cli.md` (Status Active)

1. Update command purpose text from "evict dead" to "evict target disk (live or missing)."
2. Add explicit safety rule for mixed-state rejection.
3. Add `--missing-id` scoping rule.
4. Record implementation constraint that present-device eviction behavior is shared between `remove` and `replace`.

### 3. Principles alignment
File: `docs/principles.md`

1. Update principle language minimally so command behavior reflects live+dead replace semantics without ambiguity.
2. Keep "safe-by-construction" intent semantics explicit: replacement remains transactional and uses shared safety primitives for eviction behavior.

## Acceptance Criteria
1. `braid replace` succeeds for live old disk in healthy pool.
2. `braid replace` succeeds for live old disk in topology that ends with a single-device pool.
3. dead/missing replace behavior remains intact.
4. mixed state (live old + missing devices) fails with actionable message.
5. live old + `--missing-id` fails with actionable message.
6. `old == new` fails with actionable message.
7. checkpoint resume works for replace path after forced interruption, including conversion-before-evict live path.
8. live replace path fully releases old disk semantics: removal plus best-effort mapper close warning behavior.
9. README and active decision docs match actual behavior and no longer describe replace as dead-only.
10. Full test suite passes for impacted checks (`replace-failed-disk`, `replace-live-disk`, `braid-checkpoint-opstate`, Rust unit tests).

## Assumptions / Defaults
1. `--old` does not need to exist in config; live pool membership is authoritative.
2. Disk-map remains advisory only and is not used for `--old` target resolution.
3. Checkpoint schema version remains unchanged.
4. Existing `ReplaceEvictDead` phase name/value are retained for compatibility.
5. Redundancy-loss confirmation phrase for replace is standardized as `replace without redundancy`.
6. Shared present-device eviction helper is the canonical implementation for live remove semantics across commands.
