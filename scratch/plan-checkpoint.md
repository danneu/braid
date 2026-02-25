# Plan: Checkpoint/Resume Aspirational Tests (Intent CLI)

## Goal

Define a failing-first test suite for the current checkpoint system (`/var/lib/braid/op-state.json`) that specifies robust, ideal resume behavior after interruption/reboot. These tests are expected to fail initially and expose gaps in current implementation.

## Scope

- In scope:
  - `braid add`
  - `braid remove`
  - `braid remove-missing`
  - `braid replace`
  - Checkpoint file: `/var/lib/braid/op-state.json`
- Out of scope:
  - Disk-key rename/reassignment paths (already blocked by v1 key immutability)

## Why This Exists

btrfs can resume some low-level operations (e.g. balance), but braid still needs higher-level operation intent safety:

- Was this the same operation/args?
- Is the config still compatible?
- Is pool topology still compatible?
- Is target still present/recoverable?

The aspirational tests encode those guarantees explicitly.

## Desired User-Facing Behavior (Aspirational)

When user reruns the same command after interruption/reboot:

1. braid detects interrupted op from `op-state.json`
2. braid prints explicit resume intent message
3. braid re-establishes runtime preconditions (open/mount as needed)
4. braid validates resume safety
5. braid continues unfinished phase
6. on success, checkpoint is cleared
7. on validation failure, braid fails with explicit reason (no silent unsafe continue)

## Test Strategy

- Failing-first by design.
- Favor deterministic interruption hooks over timing-based kill when possible.
- Validate both behavior and state artifact (`op-state.json`).
- Use NixOS VM tests for end-to-end semantics.
- Add Rust unit tests for checkpoint validation logic.

---

## Test Matrix (Applicable Cases Only)

### 1) Resume happy-path after interrupted add

- Setup: interrupt `braid add disk2` after checkpoint write, before long phase completes.
- Action: rerun `braid add disk2`.
- Expectation:
  - explicit resume message
  - operation completes
  - `/var/lib/braid/op-state.json` removed on success

### 2) Resume fails when target disk absent

- Setup: interrupted `braid add disk2`, then make `disk2` unavailable after reboot simulation.
- Action: rerun `braid add disk2`.
- Expectation:
  - fail with target-missing style reason
  - checkpoint preserved for retry (or archived with explicit failure metadata)

### 3) Resume rejects args mismatch

- Setup: checkpoint belongs to `braid add disk2`.
- Action: run `braid add disk3`.
- Expectation:
  - explicit op/arg mismatch rejection
  - no continuation of old checkpointed operation

### 4) Resume rejects topology drift

- Setup: interrupted operation + pool membership changes before retry.
- Action: rerun original command.
- Expectation:
  - explicit topology drift reason
  - no unsafe continuation

### 5) Resume rejects corrupted checkpoint safely

- Setup: malformed JSON in `/var/lib/braid/op-state.json`.
- Action: rerun command.
- Expectation:
  - deterministic corruption handling (clear error)
  - fail-safe behavior (never blindly continue)

### 6) Resume rejects allowed config drift (non-key-rename)

- Setup: interrupted op; change permitted config content (e.g. mount point), not disk key rename.
- Action: rerun original command.
- Expectation:
  - explicit config drift rejection
  - no continuation under changed config context

### 7) Completion cleanup contract

- Setup: successful resumed operation.
- Action: complete resume.
- Expectation:
  - active checkpoint file removed
  - final output indicates operation finished

---

## Contract Tests (Aspirational)

These likely require small product changes and should fail now:

- Checkpoint schema includes:
  - `schema_version`
  - `run_id`
  - `phase`
  - `updated_at`
- Deterministic test hooks:
  - `BRAID_TEST_FAIL_AFTER_CHECKPOINT=1`
  - `BRAID_TEST_PAUSE_AT_CHECKPOINT=1`

## Proposed Test Files

- `tests/25-braid-checkpoint-opstate.nix`
- `tests/braid-checkpoint-opstate.py`
- Additional unit tests in `cli/src/checkpoint.rs`
- `flake.nix` check registration: `braid-checkpoint-opstate`

## Acceptance Criteria for This Planning Phase

- Plan is narrow to applicable checkpoint behavior in intent CLI.
- Plan explicitly marks initial failures as expected and desirable.
- Plan excludes known non-applicable scenarios.
- Plan is implementation-ready for TDD sequence.

## Assumptions

- Intent CLI is canonical (`add/remove/remove-missing/replace`).
- We prefer explicit failures over silent fallback when resume safety checks fail.
- Reboot-resume behavior should be robust and operator-visible.

## Code tips

- Use our `atomic_write` util fn when creating and updating files on disk
