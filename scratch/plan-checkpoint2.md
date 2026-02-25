# Checkpoint Overhaul Plan: Pure Validation Core + Thin Command Wiring

## Summary
Replace the current vestigial checkpoint flow with a simpler, correctness-first design centered on a pure Rust validation core.  
The core idea is: all resume decisions are made by deterministic, table-driven unit-tested logic; command handlers only do I/O orchestration and phase transitions; VM tests stay thin and verify end-to-end wiring.  

## Scope
1. In scope:
- Redesign checkpoint schema and validation pipeline.
- Refactor mutating intent commands (`add`, `remove`, `remove-missing`, `replace`) to use shared checkpoint engine.
- Add deterministic test hooks for interruption and fixed time.
- Add high-coverage Rust unit matrix and thin VM integration tests.
- Update docs describing checkpoint invariants and behavior.

2. Out of scope:
- Porting old apply/plan checkpoint tests.

## Public Interface and Behavior Contract

### CLI behavior contract
1. On command start (`add/remove/remove-missing/replace`), if `/var/lib/braid/op-state.json` exists:
- Validate checkpoint against current invocation + current config hash + current pool fingerprint.
- If valid and same op/args: auto-resume from saved phase.
- If invalid: fail non-zero with explicit reason, do not mutate disks, do not silently continue.
- Validation must complete before any mutating `CmdRequest` is issued.

2. On successful completion of a command:
- Remove active checkpoint file.

3. On failure after checkpoint creation:
- Keep checkpoint file as-is for retry.

4. For corruption / schema mismatch:
- Fail non-zero with deterministic error code and message.
- Keep file for inspection (no auto-delete).

### Error code surface
Introduce stable error IDs used in stderr and unit tests:
1. `CHECKPOINT_CORRUPT`
2. `CHECKPOINT_SCHEMA_UNSUPPORTED`
3. `CHECKPOINT_OP_MISMATCH`
4. `CHECKPOINT_ARGS_MISMATCH`
5. `CHECKPOINT_CONFIG_DRIFT`
6. `CHECKPOINT_TOPOLOGY_DRIFT`
7. `CHECKPOINT_TARGET_MISSING`
8. `CHECKPOINT_PHASE_INVALID`
9. `CHECKPOINT_PAUSE_TIMEOUT`

### Error output format
All checkpoint contract failures must use one stderr format:
1. `error[CODE]: message`
2. `CODE` is one of the stable checkpoint error IDs.
3. Unit tests and VM tests assert this exact prefix format.

## Data Model and Core APIs

### New checkpoint schema (v1)
Implement in [checkpoint.rs](/Users/dan/Code/braid/cli/src/checkpoint.rs):
1. `schema_version: u32` (constant `1`).
2. `run_id: String` (UUID).
3. `op: OpKind` (`Add|Remove|RemoveMissing|Replace`).
4. `op_args: OpArgs` (typed enum/struct, not loose strings).
5. `phase: Phase` (typed enum; no raw numeric step in command code).
6. `created_at: String` (RFC3339).
7. `updated_at: String` (RFC3339).
8. `config_hash: String`.
9. `pool_fingerprint: PoolFingerprint`.
10. `target_snapshot: TargetSnapshot` (minimal fields needed to detect target disappearance/inconsistency).

### Pure validation core
Add pure function:
1. `validate_resume(checkpoint: &CheckpointV1, invocation: &InvocationCtx, live: &LiveCtx) -> ResumeDecision`.

Definitions:
1. `InvocationCtx`: command + args hash + config hash expected.
2. `LiveCtx`: pool fingerprint + target presence facts.
3. `ResumeDecision`:
- `ResumeFrom { phase: Phase }`
- `Reject { code: CheckpointErrorCode, reason: String }`

Rules are deterministic and side-effect free.

### Side-effect boundaries
Checkpoint module split:
1. Pure:
- Schema types.
- Hashing helpers.
- `validate_resume`.
- Phase progression helpers.
- Test hook parser (pure parse only).
2. I/O wrappers:
- `load_checkpoint_file(path) -> Result<CheckpointV1, CheckpointIoError>`
- `save_checkpoint_atomic(path, checkpoint)`
- `clear_checkpoint(path)`

Use existing `atomic_write` for writes.

### Clock abstraction
Introduce clock trait:
1. `trait Clock { fn now_rfc3339(&self) -> String; }`
2. `SystemClock` for runtime.
3. `FixedClock` for tests.
4. `created_at` set once at initial checkpoint.
5. `updated_at` set on each phase update.

## Command Integration Plan

### Shared runner flow in each mutating command
Refactor [add.rs](/Users/dan/Code/braid/cli/src/add.rs), [remove.rs](/Users/dan/Code/braid/cli/src/remove.rs), [remove_missing.rs](/Users/dan/Code/braid/cli/src/remove_missing.rs), [replace.rs](/Users/dan/Code/braid/cli/src/replace.rs):
1. Build `InvocationCtx`.
2. Probe live state and build `LiveCtx`.
3. Attempt `load + validate`.
4. If `ResumeFrom`, continue at checkpoint phase.
5. If `Reject`, return validation error with stable code.
6. If no checkpoint, start new checkpoint at first long/multi-step phase.
7. Persist phase transitions via one helper (shared API), updating `updated_at`.
8. Clear checkpoint on full success only.

Pre-mutation guard requirement:
1. Introduce a shared guard that blocks mutating `CmdRequest` execution until resume gate resolves to `ResumeFrom` or `NoCheckpoint`.
2. Add an invariant unit test asserting zero mutating requests are emitted when resume validation returns `Reject`.

### Deterministic interruption hooks
Add test-only hook points in command flow:
1. `BRAID_TEST_FAIL_AFTER_CHECKPOINT=1` causes deterministic error immediately after first checkpoint save.
2. `BRAID_TEST_FAIL_AT_PHASE=<phase_name>` causes error when entering specified phase.
3. `BRAID_TEST_PAUSE_AT_PHASE=<phase_name>` blocks until signal file exists (VM harness controlled).
4. Pause behavior must enforce a bounded timeout (`BRAID_TEST_PAUSE_TIMEOUT_SECS`, default `30`) and fail with `CHECKPOINT_PAUSE_TIMEOUT`.

Hooks must be no-op unless env var is set.

## Test Plan

### Rust unit tests (primary correctness surface)
Add table-driven suite in [checkpoint.rs](/Users/dan/Code/braid/cli/src/checkpoint.rs) tests (or dedicated module):
1. Valid resume for each op kind.
2. `op` mismatch.
3. args mismatch.
4. config hash drift.
5. topology drift.
6. target missing.
7. malformed JSON decode path.
8. unsupported `schema_version`.
9. invalid phase transition.
10. timestamp update behavior with `FixedClock`.
11. hash determinism and canonical arg serialization.
12. fingerprint normalization determinism.

Target: at least 25 matrix cases with named fixtures.

### Command-level unit tests (thin, focused)
For each mutating command:
1. Valid checkpoint resumes from non-initial phase and skips prior phases.
2. Reject decision returns explicit error code and does not run mutating command requests.
3. `FAIL_AFTER_CHECKPOINT` leaves checkpoint file present.

### VM tests (thin end-to-end wiring)
Add:
- [tests/25-braid-checkpoint-opstate.nix](/Users/dan/Code/braid/tests/25-braid-checkpoint-opstate.nix)
- [tests/braid-checkpoint-opstate.py](/Users/dan/Code/braid/tests/braid-checkpoint-opstate.py)

Scenarios:
1. One full interruption/resume flow on `add` via deterministic hook, rerun resumes and succeeds, checkpoint cleared.
2. One shared rejection flow end-to-end (`config drift`), with explicit error code and no mutation.
3. One pause-timeout flow validates bounded wait and `CHECKPOINT_PAUSE_TIMEOUT`.

All VM test files must include required What/Why/Dependencies header block.

## Flake and Check Wiring
1. Register new check `braid-checkpoint-opstate` in [flake.nix](/Users/dan/Code/braid/flake.nix).

## Documentation Updates
1. Update [docs/decisions/intent-cli.md](/Users/dan/Code/braid/docs/decisions/intent-cli.md) resumability section to match new strict contract.
2. If behavior/invariants changed materially, add or update a decision doc with explicit status (`Active`).
3. Update [README.md](/Users/dan/Code/braid/README.md) with short operator-facing resume behavior and error code examples.

## Implementation Sequence (decision-complete)
1. Introduce v1 schema types + pure `validate_resume` + enums + error codes.
2. Add clock abstraction and deterministic timestamp plumbing.
3. Add deterministic interruption/pause hook mechanism with bounded timeout.
4. Add pre-mutation guard + invariant tests (no mutating requests before resume gate).
5. Refactor `add` to new checkpoint engine and phase model.
6. Land unit tests + thin VM tests for `add` and shared rejection path.
7. Refactor `remove` using identical harness and guard.
8. Refactor `remove-missing` using identical harness and guard.
9. Refactor `replace` using identical harness and guard.
10. Add/finish full unit matrix and command-level tests for all commands.
11. Register VM check in flake.
12. Update docs and finalize acceptance checks.

## Acceptance Criteria
1. All checkpoint resume decisions are made by one pure function with no side effects.
2. Unit matrix covers all rejection classes and happy paths for all mutating commands.
3. VM test proves one interrupted-resume success path (`add`).
4. VM test proves one explicit rejection path with stable error code.
5. No silent stale-checkpoint invalidation + continue behavior remains.
6. Checkpoint writes are atomic and checkpoint is only cleared on success.
7. Tests assert `error[CODE]: message` formatting.
8. Invariant test proves no mutating `CmdRequest` occurs before resume gate resolves.

## Assumptions and Defaults
1. Default behavior is strict fail-closed on invalid checkpoints.
2. RFC3339 UTC timestamps are required.
3. Checkpoint file path remains `/var/lib/braid/op-state.json`.
4. Operator-visible messages include stable error codes for automated assertions.
