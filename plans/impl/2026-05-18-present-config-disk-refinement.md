# Refactor: refined `ConfigDisk` at the probe-to-plan boundary

## Context

`ConfigDiskState::Absent` is rejected redundantly on the plan path in both
`replace.rs` and `add.rs`. Today there are five Absent guards across the two
files but only two are load-bearing:

| Location                                 | Status      |
| ---------------------------------------- | ----------- |
| `replace.rs:1350` (`plan_replace`)       | load-bearing |
| `replace.rs:1509` (`build_replace_work_plan`)   | unreachable |
| `replace.rs:1579` (`build_replace_journal_target`) | unreachable |
| `add.rs:1515` (`plan_add`)               | load-bearing |
| `add.rs:1806` (`build_add_work_plan`)    | unreachable |

The unreachable guards force `Result` returns on
`build_replace_work_plan` and `build_replace_journal_target` for a branch
that cannot fire, infect `?` propagation up the call chain, and make
future readers prove the invariant in three places.

This refactor introduces `PresentConfigDisk` -- a refined `ConfigDisk`
that cannot represent `Absent` -- at the probe-to-plan boundary. After
the change, the "is this disk plugged in?" check fires exactly once per
planner. Builders consume the refined type and their matches become
2-arm exhaustive. `build_replace_work_plan` and
`build_replace_journal_target` become infallible.

Other `ConfigDiskState::Absent` consumers (`mount.rs`,
`enroll_key_file.rs`, `recover.rs`, `status.rs`, the `probe.rs`
producer) treat `Absent` as a semantic state with meaning and are
unaffected.

## Approach

1. Add `PresentConfigDiskState` and `PresentConfigDisk` to
   `cli/src/types.rs`, alongside a `TryFrom<ConfigDisk>` impl whose
   error returns the original `ConfigDisk` so the caller can format the
   "not present" message from `name` / `by_id_path`.
2. In `plan_replace`, convert `ConfigDisk` -> `PresentConfigDisk` right
   after `probe_config_disk` returns, before any of the keyfile /
   `new_uuid` matches. All three downstream matches on `new_probed.state`
   collapse to 2-arm exhaustive on `PresentConfigDiskState`.
3. In `plan_add`, replace the Absent loop with a
   `Vec<ConfigDisk> -> Vec<PresentConfigDisk>` conversion. Keep the
   existing `names[i]` / `by_ids[i]` index access in `build_add_work_plan`
   unchanged -- this refactor only narrows the state, not the loop shape.
4. Tighten builder signatures: `ReplaceWorkPlanInput.new_probed` and
   `AddStepsInput.probed` take the refined type;
   `build_replace_work_plan` and `build_replace_journal_target` become
   infallible.
5. Update test fixtures that construct `ConfigDisk` for direct builder
   calls; update the `#[cfg(test)] ReplaceWorkPlanTestInput` and
   `replace_work_plan_for_test` helper; update `AddPlan.probed` type,
   `build_add_credential_prelude` matches, and the `cloned_disk_probed`
   / `probed_present_luks` helpers.
6. Add focused Rust regression tests pinning the exact absent-disk
   validation messages and the `PlanFailure.notes` preservation
   contract for both `plan_replace` and `plan_add`, plus a unit test
   for `TryFrom<ConfigDisk> for PresentConfigDisk` Err identity.

## Changes

### 1. `cli/src/types.rs` -- new refined types

After the existing `ConfigDiskState` (line 446), add:

```rust
/// `ConfigDiskState` minus `Absent`. The planner-side invariant after the
/// top-level probe rejects an unplugged disk. Builders consume this so
/// the "is the disk plugged in?" check lives at exactly one place per
/// command and downstream matches are exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentConfigDiskState {
    PresentNotLuks,
    PresentLuks {
        uuid: LuksUuid,
        label: Option<String>,
        mapper_open: bool,
    },
}

/// `ConfigDisk` after the planner's presence check. Carries the same
/// identity fields so downstream code reads `name` / `by_id_path`
/// directly off the refined value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentConfigDisk {
    pub name: DiskName,
    pub by_id_path: ByIdPath,
    pub state: PresentConfigDiskState,
}

impl TryFrom<ConfigDisk> for PresentConfigDisk {
    /// Returns the original `ConfigDisk` so the caller can format the
    /// "not present" diagnostic from `name` and `by_id_path` without
    /// holding a separate parallel reference.
    type Error = ConfigDisk;

    fn try_from(cd: ConfigDisk) -> Result<Self, ConfigDisk> {
        // Destructure once so we can move `state` into the match while
        // still owning `name` / `by_id_path` for every arm (including
        // the `Absent` arm that reconstructs the original `ConfigDisk`).
        // Matching on `cd.state` directly partially-moves `cd`, which
        // would make `Err(cd)` a compile error.
        let ConfigDisk { name, by_id_path, state } = cd;
        match state {
            ConfigDiskState::Absent => Err(ConfigDisk {
                name,
                by_id_path,
                state: ConfigDiskState::Absent,
            }),
            ConfigDiskState::PresentNotLuks => Ok(PresentConfigDisk {
                name,
                by_id_path,
                state: PresentConfigDiskState::PresentNotLuks,
            }),
            ConfigDiskState::PresentLuks { uuid, label, mapper_open } => {
                Ok(PresentConfigDisk {
                    name,
                    by_id_path,
                    state: PresentConfigDiskState::PresentLuks { uuid, label, mapper_open },
                })
            }
        }
    }
}
```

Add a unit test next to the existing `ConfigDiskState` tests (mod `tests` at
`types.rs:452`) that verifies the `Err` path preserves `name` and
`by_id_path` so the planner's error formatting is locked.

### 2. `cli/src/replace.rs`

- **Insert refinement right after the probe** (currently `replace.rs:1273-1284`):

  ```rust
  let new_probed: PresentConfigDisk = match PresentConfigDisk::try_from(new_probed) {
      Ok(p) => p,
      Err(orig) => {
          return Err(PlanFailure::with_notes(
              notes,
              ReplaceError::Validation(format!(
                  "new disk '{}' ({}) is not present. Is it plugged in?",
                  orig.name, orig.by_id_path
              )),
          ));
      }
  };
  ```

  `notes` at this point already contains any `preflight_notes` extended
  in at `replace.rs:1189` (`preflight::require_mutation_preflight`), so
  `with_notes(notes, ...)` preserves them on the rejection path -- the
  same contract the existing Absent rejection at line 1350 maintains.

- **Remove the Absent arm at `replace.rs:1350-1358`**. The `new_uuid`
  match at line 1347 becomes 2-arm exhaustive over
  `PresentConfigDiskState`. Update the doc comment at line 1346 to
  drop the "Absent: rejected by `build_replace_work_plan`" bullet --
  replace with "Absent: rejected at the probe boundary above."

- **Update the keyfile-diagnostics check** at line 1289 to match on
  `PresentConfigDiskState::PresentNotLuks`.

- **Update the `resolved_enroll_key_file` match** at line 1313 to use
  `PresentConfigDiskState::PresentLuks { .. }` in the typed arm.

- **`ReplaceWorkPlanInput.new_probed`** (`replace.rs:1477`): change type
  to `PresentConfigDisk`.

- **`build_replace_work_plan`** (`replace.rs:1493`): return type changes
  to `ReplaceWorkPlan` (drop `Result<_, ReplaceError>`). Remove the
  Absent arm at lines 1509-1514; the match on
  `input.new_probed.state` becomes 2-arm exhaustive over
  `PresentConfigDiskState`. The `?` on
  `build_replace_journal_target(...)` at line 1503 becomes a plain call.

- **`build_replace_journal_target`** (`replace.rs:1565`): return type
  changes to `journal::ReplaceJournalTarget`. Take
  `&PresentConfigDisk` (or `&PresentConfigDiskState`) instead of
  `&ConfigDisk`. Remove the Absent arm at lines 1579-1584.

- **Call site at `replace.rs:1384-1404`**: collapse the
  `match build_replace_work_plan(...) { Ok => p, Err => return ... }`
  to a direct assignment.

- **Update the doc comment at `replace.rs:1486-1488`** about
  `enroll_key_file`: drop the "and `Absent` paths" clause since the
  type can no longer be `Absent` at this point.

- **Test helpers**:
  - `ReplaceWorkPlanTestInput.new_probed` (`replace.rs:1740`): change
    type to `&'a PresentConfigDisk`.
  - `replace_work_plan_for_test` (`replace.rs:1751`): rewrite the
    wildcard match at lines 1764-1767 as a 2-arm exhaustive match on
    `PresentConfigDiskState`. Drop the `Result` return type now that
    `build_replace_work_plan` is infallible.
  - `new_probed_not_luks` helper (`replace.rs:2798`): change return
    type from `ConfigDisk` to `PresentConfigDisk`.
  - Fixtures that build `ConfigDisk { state: ConfigDiskState::Present* }`
    for direct builder calls (`replace.rs:2291`, `:2356`, `:2818`,
    `:2855`, `:3569`) -- rewrite to `PresentConfigDisk { state:
    PresentConfigDiskState::Present* }`. Run `rg 'ConfigDisk \{'
    cli/src/replace.rs` immediately before implementation to confirm
    the full list.
  - Named tests that call `build_replace_journal_target` directly
    (`build_replace_journal_target_records_fresh_luks_target` at
    `replace.rs:2820`, `build_replace_journal_target_records_existing_luks_target`
    at `replace.rs:2859`) -- adapt the input shape and drop `.expect(...)`
    on the now-direct return.

### 3. `cli/src/add.rs`

- **Replace the Absent loop at `add.rs:1514-1521`** with:

  ```rust
  let probed: Vec<PresentConfigDisk> = probed
      .into_iter()
      .map(|p| {
          PresentConfigDisk::try_from(p).map_err(|orig| {
              PlanFailure::empty(AddError::Validation(format!(
                  "disk '{}' ({}) is not present. Is it plugged in?",
                  orig.name, orig.by_id_path
              )))
          })
      })
      .collect::<Result<_, _>>()?;
  ```

  Error message reads from `orig.name` / `orig.by_id_path` (identical
  text to today's `names[i]` / `by_ids[i]`).

- **`any_needs_format` check in `plan_add`** (`add.rs:1565-1567`):
  update the `matches!` pattern to `PresentConfigDiskState::PresentNotLuks`.

- **`AddPlan.probed`** (`add.rs:797`): pub field, change type to
  `Vec<PresentConfigDisk>`. The struct literal at `add.rs:1617-1627`
  takes the now-refined `probed` value -- no separate change there
  beyond the type. No external consumers of `AddPlan.probed` were
  found in `cli/src/` (verified via `grep -rn '\.probed'`).

- **`AddStepsInput.probed`** (`add.rs:1662`): change type to
  `&'a [PresentConfigDisk]`.

- **`build_add_credential_prelude`** (`add.rs:1673-1719`): three
  matches need updating:
  - Line 1682: `matches!(probed.state, ConfigDiskState::PresentNotLuks)`
    -> `PresentConfigDiskState::PresentNotLuks`.
  - Lines 1687-1689: `any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks))`
    -> `PresentConfigDiskState::PresentNotLuks`.
  - Line 1708: `let ConfigDiskState::PresentLuks { uuid, .. } = &probed.state`
    -> `let PresentConfigDiskState::PresentLuks { uuid, .. } = &probed.state`.

- **`build_add_work_plan`** (`add.rs:1788`): keep
  `Result<AddWorkPlan, AddError>` (duplicate-UUID rejection at
  lines 1830-1841 remains). Remove the Absent arm at lines 1806-1811;
  the match at line 1805 becomes 2-arm exhaustive over
  `PresentConfigDiskState`. The loop body at lines 1799-1801 still
  reads `name = &input.names[i]` and `by_id = input.by_ids[i]` --
  unchanged.

- **Test helpers**:
  - `probed_present_luks` (`add.rs:2466`): change return type from
    `ConfigDisk` to `PresentConfigDisk`; construct
    `PresentConfigDisk { state: PresentConfigDiskState::PresentLuks { .. } }`.
    Already-known call sites: lines 2481, 2517, 2549, 2585, 2887, 5619,
    5708 (no change at the call site because the helper return type
    flows through).
  - `cloned_disk_probed` (`add.rs:7768-7780`): change return type from
    `ConfigDisk` to `PresentConfigDisk`. Call sites: lines 7819, 7824,
    8200+ (rg before implementation to confirm full list).
  - Direct `ConfigDisk { state: ConfigDiskState::Present* }` fixtures
    feeding `AddStepsInput.probed` or `AddPlan.probed`: rewrite to the
    refined type. Known sites: `add.rs:2617`, `:2672`, `:3363`, `:5468`,
    `:5547`, `:5764`, `:7924-7938` (vector of three), `:8200`. Run
    `rg 'ConfigDisk \{' cli/src/add.rs` immediately before implementation
    and convert every site that flows into a builder or `AddPlan` literal.
  - `AddPlan` literal constructions at `add.rs:2738-2748` and
    `add.rs:3387` (tests): the `probed:` field assignment now carries
    `Vec<PresentConfigDisk>` -- the local variable on either side
    must be refined to match.

### 4. New regression tests

The existing test suite covers the builders' success paths but does
not pin the absent-disk rejection messages or the note-preservation
contract on the planner-level path. The refactor moves the rejection
site, so these need explicit regression coverage:

- **`cli/src/types.rs`** -- add `try_from_config_disk_absent_preserves_identity`:
  build `ConfigDisk { state: ConfigDiskState::Absent, name, by_id_path }`,
  call `PresentConfigDisk::try_from`, assert the `Err` is a
  `ConfigDisk` whose `name` and `by_id_path` equal the inputs and
  whose `state` is `Absent`. Locks the conversion contract that the
  planner error messages rely on.

- **`cli/src/replace.rs`** -- add `plan_replace_rejects_absent_new_disk_with_exact_message`:
  set up a mounted-btrfs pool, an old member resolvable in membership,
  a `--new` disk that probes as `Absent`. The fixture must satisfy two
  orthogonal constraints:
  1. The new disk's by-id path must NOT appear in `MockFs::storage(...)`.
     `probe_config_disk` (`cli/src/probe.rs:169-174`) returns
     `ConfigDiskState::Absent` exactly when `fs.exists(by_id)` is false,
     and `MockFs::exists`
     (`cli/src/test_fixtures/shared.rs:160-162`) is true only for the
     paths in the vec. Listing the by-id path defeats the test by
     forcing the probe down the LUKS-uuid branch instead.
  2. `with_excl_op("device add\n")` must be set so that
     `preflight::require_mutation_preflight`
     (`cli/src/preflight.rs:502-515`) pushes
     `PreviewNote::Info("waiting for in-flight device add to finish...")`
     into `notes`. On the clean sysfs path the returned vec is empty
     and the preservation assertion would pass vacuously.

  Resulting fixture:
  ```rust
  let fs = MockFs::storage(vec![]).with_excl_op("device add\n");
  ```
  (No new-disk paths; runner-side pool state still comes from
  `PoolFixture::two_disk_healthy` / `ReplacementPool::two_disk_healthy`
  as in `plan_replace_preflight_busy_op_becomes_info_note` at
  `replace.rs:4585-4621`.)

  Then assert:
  - `plan_replace` returns `Err(PlanFailure { notes, error })`.
  - `error` is `ReplaceError::Validation(body)` with `body` equal to
    `"new disk '<name>' (<by_id>) is not present. Is it plugged in?"`
    (exact text, formatted with the test inputs).
  - `notes.iter().any(|n| matches!(n, PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")))`
    is true -- pins the `with_notes(notes, ...)` preservation contract
    that the moved rejection point must keep.

- **`cli/src/add.rs`** -- add `plan_add_rejects_absent_new_disk_with_exact_message`:
  set up a mounted pool and a single `--new` disk that probes as
  `Absent`. Assert:
  - `plan_add` returns `Err`.
  - The inner `AddError::Validation` body equals
    `"disk '<name>' (<by_id>) is not present. Is it plugged in?"`.
  - The returned `PlanFailure.notes` is empty (the existing rejection
    uses `PlanFailure::empty(...)`; the refactor must preserve this).

These tests live in the existing `#[cfg(test)] mod tests` blocks and
reuse the project's `PoolFixture` / `AddPlanTestRunner` test
infrastructure where applicable. Each test gets the standard
three-section `// Intent: / Why it exists: / Scenario:` preamble per
`AGENTS.md` test conventions.

### 5. Out of scope

- `mount.rs`, `enroll_key_file.rs`, `recover.rs`, `status.rs` -- Absent
  is a meaningful state for these.
- `remove.rs`, `remove_missing.rs` -- do not probe target disks at all.
- The `probe.rs` producer -- still returns `ConfigDisk` (the refinement
  happens at the planner boundary, not at the probe boundary).
- The loop-index access in `build_add_work_plan` (`add.rs:1799-1801`)
  staying unchanged. Switching to `p.name` / `p.by_id_path` is a
  separate cleanup, not folded in here.

## Files modified

- `cli/src/types.rs` -- add `PresentConfigDiskState`, `PresentConfigDisk`,
  `TryFrom<ConfigDisk> for PresentConfigDisk`, and the `try_from_*`
  identity unit test.
- `cli/src/replace.rs` -- probe-boundary conversion; remove 2 Absent
  arms; tighten 3 function signatures (`ReplaceWorkPlanInput`,
  `build_replace_work_plan`, `build_replace_journal_target`); update
  `ReplaceWorkPlanTestInput` and `replace_work_plan_for_test`; update
  the `new_probed_not_luks` helper and direct `ConfigDisk` fixtures;
  update 2 stale doc comments; add the
  `plan_replace_rejects_absent_new_disk_with_exact_message` regression
  test.
- `cli/src/add.rs` -- probe-boundary conversion; remove 1 Absent arm;
  retype `AddPlan.probed` and `AddStepsInput.probed`; update three
  matches inside `build_add_credential_prelude` and one inside
  `plan_add` (`any_needs_format`); update the `probed_present_luks`
  and `cloned_disk_probed` helpers; update direct `ConfigDisk`
  fixtures and `AddPlan` literal constructions in tests; add the
  `plan_add_rejects_absent_new_disk_with_exact_message` regression
  test.

## Verification

1. **Compile**: `cargo check -p braid-cli` -- the type system should
   prove the invariant. Any missed match arm is a compile error, not a
   runtime surprise.
2. **Rust unit tests**: `just test-rust` -- covers the `TryFrom` Err
   conversion test, the existing `build_replace_journal_target_records_*`
   tests, and all `replace.rs` / `add.rs` fixture-driven tests.
3. **NixOS VM tests, scoped**:
   - `just test-vm replace-live replace-missing replace-keyfile` --
     end-to-end replace command paths.
   - `just test-vm add add-warnings add-pool-keyfile` -- end-to-end
     add command paths.
4. **Regression coverage** (added in section 4 above): the new
   `plan_replace_rejects_absent_new_disk_with_exact_message` and
   `plan_add_rejects_absent_new_disk_with_exact_message` tests pin the
   exact error text and note-preservation behavior, so the byte-for-byte
   contract is verified by the test suite rather than manual inspection.
5. **Full test pass before commit**: `just test-rust && just test-vm`.

## Risks and notes

- **Behavior preserved on the early-rejection move in `plan_replace`**:
  the keyfile-diagnostics block (currently line 1289) is gated on
  `PresentNotLuks` and is a no-op on `Absent`; the
  `resolved_enroll_key_file` match (line 1311) has an `Absent`
  fall-through that is also a no-op. No notes pushed, no commands
  emitted between probe and reject. Rejecting `Absent` immediately
  after probe is externally indistinguishable from rejecting at line
  1350.
- **No other duplicate-guard sites in the codebase**: audit confirmed
  `remove.rs` / `remove_missing.rs` do not probe their targets;
  `mount.rs`, `enroll_key_file.rs`, `recover.rs`, `status.rs` all
  treat `Absent` as a legitimate state.
- **First refinement pattern in the codebase**. Establishes a
  convention (refined sibling type + `TryFrom` returning the
  original) that future probe-to-plan narrowings can follow.
