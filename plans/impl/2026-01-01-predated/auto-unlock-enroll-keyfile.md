# Plan: Refactor `enroll-key-file` to two-phase validate/mutate

## Context

The current `cmd_enroll_key_file` mixes validation and mutation in a single loop: for each disk it checks slot state, then immediately calls `luksAddKey`. If disk 3 has a slot conflict, disks 1-2 have already been mutated. This violates fail-close: a predictable preflight conflict should prevent all mutations.

The refactor splits into three strict layers — discovery, planning, execution — guaranteeing zero mutations for preflight-detectable conflicts (e.g. slot-1 occupied). Runtime failures during apply may still be partial.

Preserve current skip semantics for absent/non-LUKS disks; two-phase safety applies to present LUKS candidates.

## Architecture: three functions, strict separation

### `discover_enrollment_candidates(runner, fs, config)` → `Vec<(String, DiskConfig)>`
- **Sole responsibility**: topology discovery
- Iterates `config.disks()`, calls `probe::probe_config_disk` for each
- Absent → `eprintln!("skip: {} not present", key)`, continue
- PresentNotLuks → `eprintln!("skip: {} not LUKS-formatted", key)`, continue
- PresentLuks → collect into candidate list
- Errors if zero candidates found
- No passphrase needed, no keyfile checks, no mutation

### `plan_enrollment(runner, candidates, key_file_path, passphrase)` → `Vec<DiskEnrollAction>`
- **Sole responsibility**: passphrase verification + per-disk keyfile/slot classification
- Verifies passphrase once against first candidate disk; fails on wrong passphrase
- For each candidate:
  - `verify_key_file` → true: `AlreadyEnrolled`
  - else `check_key_slot(slot 1)` → `Empty`: `NeedsEnroll`, `Occupied`: fail with conflict
- Returns an immutable typed plan
- Prints preflight summary:
  ```
  ok: disk1 — keyfile already enrolled
  enroll: disk2 — will add keyfile to slot 1
  ```
- No mutation

### `apply_enrollment(runner, plan, passphrase, key_file_path)` → `Result<()>`
- **Sole responsibility**: mutation
- Consumes only `NeedsEnroll` items from the plan; no reclassification
- Calls `luks::enroll_key_file` for each
- Prints per-disk confirmation and final summary:
  ```
  ok: disk2 — keyfile enrolled in slot 1
  done: 2 enrolled, 1 already had keyfile
  ```

### `cmd_enroll_key_file` orchestrator (same public signature)
1. Validate keyfile exists and is a regular file (unchanged)
2. `discover_enrollment_candidates(runner, fs, config)` — topology with skip semantics
3. `luks::read_passphrase(...)` — only prompted after discovery succeeds
4. `plan_enrollment(runner, &candidates, key_file_path, &passphrase)` — read-only classification
5. `apply_enrollment(runner, &plan, &passphrase, key_file_path)` — mutation

### `DiskEnrollAction` enum
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskEnrollAction {
    AlreadyEnrolled { key: String, disk: DiskConfig },
    NeedsEnroll { key: String, disk: DiskConfig },
}
```
Uses `DiskConfig` (which contains `by_id: ByIdPath`) rather than raw `String` to preserve codebase newtype conventions.

## File changes

### `cli/src/enroll_key_file.rs` (sole production file)

Full rewrite of function body. Add `DiskEnrollAction` enum, `discover_enrollment_candidates`, `plan_enrollment`, `apply_enrollment`. Rewrite `cmd_enroll_key_file` as thin orchestrator.

#### Unit tests

**`discover_enrollment_candidates` tests** (using `MockRunner` + mock probe):
- Two disks present and LUKS → returns both as candidates
- One absent, one present → returns only the present disk (skip semantics preserved)
- One PresentNotLuks, one PresentLuks → returns only the LUKS disk
- All absent / all non-LUKS → error "no present LUKS disks found"

**`plan_enrollment` tests** (using `MockRunner`):
- All disks need enroll (slot 1 empty, keyfile doesn't verify) → all `NeedsEnroll`
- All disks already enrolled (keyfile verifies) → all `AlreadyEnrolled`
- Mixed: some enrolled, some need enroll
- Wrong passphrase → error
- Slot 1 conflict (occupied by unknown key) → error

**`apply_enrollment` tests** (using `MockRunner`):
- Plan with only `NeedsEnroll` → `enroll_key_file` called for each
- Plan with only `AlreadyEnrolled` → no `enroll_key_file` calls, returns Ok
- Mixed plan → `enroll_key_file` called only for `NeedsEnroll` items

### `tests/cli/braid-enroll-key-file.py`

Add one e2e subtest for the real regression this refactor fixes: multi-disk pool where a later disk has slot-1 occupied by an unknown key. Assert the command fails **and** no earlier disk was newly enrolled (i.e. preflight caught the conflict before any mutation).

### `README.md`

Update the `enroll-key-file` section to note that slot conflicts are now detected before any enrollment begins (preflight).

## Existing code reused as-is

- `luks::verify_passphrase` (`cli/src/luks.rs:122`)
- `luks::verify_key_file` (`cli/src/luks.rs:217`)
- `luks::check_key_slot` (`cli/src/luks.rs:254`)
- `luks::enroll_key_file` (`cli/src/luks.rs:230`)
- `luks::read_passphrase` (`cli/src/luks.rs:32`)
- `probe::probe_config_disk` (`cli/src/probe.rs`)
- `LUKS_SLOT_KEYFILE` constant (`cli/src/luks.rs:12`)

## Verification

1. `just test-rust` — unit tests pass (plan_enrollment tests)
2. `just test braid-enroll-key-file` — existing tests pass + new preflight-conflict regression test
3. `just test braid-add-enroll-key-file` — add --enroll-key-file path unaffected
