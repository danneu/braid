# Plan: KeyFilePath / HeaderBackupPath newtypes (born at source)

## Context

`braid`'s LUKS-enrollment render code repeatedly places two `&Path` values
side by side:

- the **keyfile** whose bytes get enrolled into LUKS slot 1
  (`CmdRequest::CryptsetupLuksAddKeyFile.key_file_path`), and
- the **header-backup destination** the post-enrollment LUKS header is
  written to (`CmdRequest::CryptsetupLuksHeaderBackup.backup_path`).

Both are raw paths feeding `CmdRequest`s, so transposing them **compiles
silently**. The same pattern is duplicated across **7 render sites in 5
files**:

| Site | File | Notes |
|---|---|---|
| Fresh arm (inline) | `add.rs#render_steps` | not routed through the helper |
| `push_returned_disk_enrollment_steps` | `add.rs` | OpenRecoverable + ClosedPresentLuks call it |
| `ReplaceTargetPrep::FreshLuks` arm | `replace.rs` | inline copy of the helper |
| `ReplaceTargetPrep::ExistingLuks` arm | `replace.rs` | inline copy of the helper |
| `RecoverableBraidLabeled` replay | `recover.rs` | inline |
| `FreshLuks` replay | `recover.rs` | inline |
| standalone `braid enroll` render | `enroll_key_file.rs` | inline |

**Severity, stated precisely:** today the swap corrupts the **dry-run
preview** (ADR-022), not live mutation -- the executors
(`add.rs` Pass 2/3, `replace.rs`, `recover.rs`, `enroll_key_file.rs`) pass the
keyfile to `luks::enroll_key_file(kf)` and compute the backup path *inside*
`backup_luks_header_post_mutation` / `luks_header_backup_path`, so the two
paths are never adjacent at execution. A corrupted preview is still an ADR-022
correctness defect (the preview is the operator's pre-flight safety contract),
and the worst case ("enroll the backup destination as key material, overwrite
the keyfile with the header backup") becomes live the moment any executor is
refactored to consume these rendered steps. Two same-typed `&Path`s in a
destructive-adjacent path are a latent hazard worth removing structurally.

**Decision (chosen by the user):** **C -- born at source.** Mint each role
type at its single definitional origin so **no raw `PathBuf` form of either
role survives anywhere** to be transposed. This mirrors braid's existing
"type at the source" pattern -- `mapper_name()` returns `MapperName`,
`luks_label_for()` returns `LuksLabel`, `LuksUuid::new_v4()` mints identity --
and the construct-safe `CredentialVerifyTarget` precedent (commit `02d03253`):
private fields, minted only through a blessed constructor. Options A (helper
only) and B (planners only) were rejected because they leave the same hazard
raw in sibling files, making the type's invariant globally false -- a
false-safety island.

## The two newtypes

### `HeaderBackupPath` -- minted only at its source (`luks.rs`)

Colocated with `luks_header_backup_path` in `cli/src/luks.rs`, **private
field, no public constructor** -- so the compiler guarantees it can only be
born from the one function that defines the `<headers_dir>/<mapper>.luksheader`
convention. This is the construct-safe `CredentialVerifyTarget` pattern applied
to a path.

```rust
/// The `<headers_dir>/<mapper>.luksheader` destination braid writes the
/// post-enrollment LUKS header backup to. A distinct type from
/// `KeyFilePath` so the slot-1 key source and the backup destination --
/// which sit side by side in every enrollment render -- cannot be
/// transposed. Minted only by `luks_header_backup_path` (private field,
/// no public ctor), the single definitional source, like `mapper_name`
/// mints `MapperName`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HeaderBackupPath(PathBuf); // tuple field private => only `luks` can mint

impl HeaderBackupPath {
    /// Borrow as `&Path` for argv/`display()` and the atomic-rename writer.
    pub fn as_path(&self) -> &Path { &self.0 }
}
impl fmt::Display for HeaderBackupPath { /* self.0.display() */ }
```

The type is **`pub`** (matching every sibling role-newtype -- `ByIdPath`,
`MapperName`, `MountPoint`, `LuksUuid`, `CredentialVerifyTarget` are all
`pub struct`), but the tuple field stays private, so construction is confined
to the `luks` module where `luks_header_backup_path` lives. Making the type
`pub` is what keeps the `pub` writers (`backup_luks_header`,
`backup_luks_header_post_mutation`) from triggering `private_interfaces` once
they return it -- see the visibility note under "What stays raw".

- No `serde`: planner structs derive only `Debug, Clone`, and the journal
  does **not** store the header-backup path (verified: only `enroll_key_file`
  appears in `AddJournalMode`).
- `luks_header_backup_path(headers_dir, mapper) -> HeaderBackupPath`.
- Propagate the return type through the writers `backup_luks_header_to`,
  `backup_luks_header`, `backup_luks_header_post_mutation` (callers only
  `.display()` the result for `eprintln`); inside `backup_luks_header_to`
  use `.as_path()` where a `&Path` is needed (`into_os_string`,
  `durable_rename`).

### `KeyFilePath` -- minted at keyfile resolution (`types.rs`)

Lives in `cli/src/types.rs` alongside the other role-wrappers (`ByIdPath`,
`MountPoint`, `MapperName`). The type is **`pub`** to match the sibling
convention and to keep the `pub` `luks::enroll_key_file` from leaking a
less-visible type in its signature; minting is controlled instead by keeping
the constructor `pub(crate)` (the same shape as `LuksUuid`: `pub struct`,
construction only via `parse`/`new_v4`). Unlike the header path it has several
legitimate mint points (existing-LUKS resolution, fresh-disk direct pass, the
`enroll` command), so a single `pub(crate) fn new` is appropriate.
`#[serde(transparent)]` keeps the journal wire format byte-identical.

```rust
/// The operator-supplied keyfile whose bytes braid enrolls into LUKS
/// slot 1. A distinct type from `HeaderBackupPath` so the two paths that
/// sit side by side in every enrollment render cannot be transposed.
/// Minted where a validated keyfile becomes "the keyfile to enroll"; the
/// raw CLI path stays `&Path` (there it is operator input, not yet a role).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyFilePath(PathBuf); // tuple field private => mint only via `new`

impl KeyFilePath {
    pub(crate) fn new(path: PathBuf) -> Self { Self(path) }
    /// Borrow as `&Path` for argv/`display()`, validation/probe gates, and
    /// `luks::enroll_key_file`.
    pub fn as_path(&self) -> &Path { &self.0 }
}
impl fmt::Display for KeyFilePath { /* self.0.display() */ }
```

## What stays raw (intentionally)

`KeyFilePath` is minted **once** at each validated/resolved boundary and then
threaded; everything below stays on raw `&Path` because it is a gate, a probe,
or pre-mint CLI input -- never a place where the keyfile sits beside a
header-backup path:

- `AddParams::enroll_key_file` / `ReplaceParams::enroll_key_file` /
  `EnrollKeyFileParams::key_file_path`: `Option<&'a Path>` / `&'a Path` --
  raw CLI input, not yet a role.
- `validate_key_file_path` / `luks::validate_user_keyfile_path` -- fail-fast
  validation gates over raw input; run before minting. (Folding the mint into
  them parse-don't-validate style is possible but widens the change; keep them
  gates.)
- `luks::verify_key_file`, `enroll_key_file::probe_keyfile_enrollment`,
  `enroll_key_file::plan_single_disk_enrollment`, `generate_key_file` /
  `validate_generated_keyfile_target` -- probes/generators that act on a
  filesystem path; callers pass `key_file.as_path()`.

**Visibility (resolves the public-API-leak hazard):** because the newtypes are
`pub` (above), the `pub` `luks` helpers (`enroll_key_file`,
`backup_luks_header`, `backup_luks_header_post_mutation`) name only `pub` types
in their signatures -- **no `private_interfaces` warning, no helper-visibility
change needed.** (`cli` is a lib+bin with a `[lints]` table, so a `pub` fn
naming a `pub(crate)` type would warn.) Tightening those helpers to `pub(crate)`
to reflect their crate-internal use is a reasonable but **separate** cleanup --
out of scope here.

## Keyfile threading (the full post-validation flow)

Once `luks::enroll_key_file` accepts `&KeyFilePath`, every function on the path
from a mint point to that call must carry `&KeyFilePath` -- otherwise it either
fails to compile or invites leaf-level re-wrapping that defeats born-at-source.
The complete thread:

- **`add` (`add.rs`):** mint in `resolve_existing_luks_enroll` (`-> Option<KeyFilePath>`)
  and the Fresh construction; the planner structs carry it; Pass 2/3 hand
  `&KeyFilePath` to `luks::enroll_key_file`.
- **`replace` (`replace.rs`):** mint at `resolved_enroll_key_file` (~line 1462)
  and the fresh pass; both execute arms hand `&KeyFilePath` to
  `luks::enroll_key_file`.
- **standalone `enroll` (`enroll_key_file.rs`):** after the validate/generate
  gate succeeds, mint `KeyFilePath` once, then change
  `apply_enrollment(.., key_file: &KeyFilePath, ..)` and
  `compile_enroll_steps(.., key_file: &KeyFilePath, ..)` to thread it --
  `compile_enroll_steps` renders `key_file.as_path()` into the addKey step and
  takes the backup destination from `luks_header_backup_path` (now
  `HeaderBackupPath`). `EnrollKeyFileParams::key_file_path` stays `&Path`
  (pre-mint input).
- **recovery replay (`recover.rs`):** the keyfile arrives **already typed**
  from the journal (`AddJournalMode`/`ReplaceJournalMode` now carry
  `Option<KeyFilePath>`), so change `ensure_keyfile_enrolled(.., key_file:
  &KeyFilePath, ..)` and its replay call sites (~2472/2538/2947/3009) to pass
  the typed value through; inside, gates/probes use `key_file.as_path()` and
  `enroll_key_file` gets `&KeyFilePath`. Both replay render arms render
  `key_file.as_path()`.

## Files and changes

All edits are mechanical: a return-type change, field-type changes, mint at
the resolution points, and `.as_path()`/`.display()` at consumption.

1. **`cli/src/types.rs`** -- define `KeyFilePath` (+ `Display`, transparent
   serde).

2. **`cli/src/luks.rs`** -- define `HeaderBackupPath`; change
   `luks_header_backup_path` return to `HeaderBackupPath`; propagate through
   `backup_luks_header_to` / `backup_luks_header` /
   `backup_luks_header_post_mutation`; change `enroll_key_file`'s keyfile
   param from `&Path` to `&KeyFilePath` (6 callers, all already typed).

3. **`cli/src/add.rs`**
   - Planner struct fields: `FreshLuksTarget`, `RecoverableBraidTarget`,
     `ClosedPresentLuksCandidate` -> `enroll_key_file: Option<KeyFilePath>`,
     `header_backup_path: HeaderBackupPath`.
   - Mint keyfile: `resolve_existing_luks_enroll(...) -> Result<Option<KeyFilePath>, _>`
     (wrap the `NeedsEnroll` arm); Fresh construction
     `input.enroll_key_file.map(|p| KeyFilePath::new(p.to_path_buf()))`.
   - `header_backup_path` fields now receive `HeaderBackupPath` straight from
     `luks_header_backup_path` -- no wrapping needed.
   - `push_returned_disk_enrollment_steps(.., key_file: &KeyFilePath,
     header_backup_path: &HeaderBackupPath)` and both call sites; the Fresh
     inline arm consumes the typed fields. Execution path (Pass 2/3) passes
     `&KeyFilePath` to `luks::enroll_key_file`; `eprintln` uses `.display()`.

4. **`cli/src/replace.rs`** -- `ReplaceTargetPrep::{FreshLuks,ExistingLuks}`
   fields typed; mint at the resolution analog (`resolved_enroll_key_file`,
   ~line 1462) and the fresh pass; both inline render arms consume typed
   values; execution arms pass `&KeyFilePath` to `luks::enroll_key_file`.
   - *Adjacent cleanup (optional, out of scope):* the two inline arms are a
     copy of `push_returned_disk_enrollment_steps`; deduping them into the
     shared helper is worthwhile but orthogonal -- the newtype closes the
     hazard with or without it.

5. **`cli/src/recover.rs`** -- both replay render arms: keyfile arrives typed
   from the journal (`AddJournalMode`), backup destination typed from
   `luks_header_backup_path`; render via `.as_path()`/`.display()` as today.
   `ensure_keyfile_enrolled(.., key_file: &KeyFilePath, ..)` and its replay
   call sites thread the typed value (see "Keyfile threading"); its internal
   `validate_user_keyfile_path`/`verify_key_file` gates take `key_file.as_path()`
   and `luks::enroll_key_file` takes `&KeyFilePath`.

6. **`cli/src/enroll_key_file.rs`** -- thread `&KeyFilePath` through
   `compile_enroll_steps` and `apply_enrollment` (see "Keyfile threading"),
   minting once after the validate/generate gate; backup destination typed from
   `luks_header_backup_path`. `plan_single_disk_enrollment`,
   `probe_keyfile_enrollment`, `validate_key_file_path`, `generate_key_file`,
   and `EnrollKeyFileParams::key_file_path` keep raw `&Path` (gates/probes/
   pre-mint input).

7. **`cli/src/journal.rs`** -- `enroll_key_file: Option<PathBuf>` ->
   `Option<KeyFilePath>` in the `AddJournalMode` and `ReplaceJournalMode`
   variants that carry it. Transparent serde => **wire format unchanged**.
   Update the literal `PathBuf::from(...)` constructions in the roundtrip and
   serde-drift tests to `KeyFilePath::new(PathBuf::from(...))`.

## Verification

This is a **behavior-preserving refactor**: every render must emit the exact
same `CmdRequest`s as before. Coverage is the existing suite; the newtypes are
the new *structural* guarantee.

1. `just test-rust` -- existing render tests must pass unchanged, proving
   rendering is preserved:
   - `add.rs`: `dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey`,
     `dry_run_render_closed_present_luks_with_enroll_renders_addkey_and_backup`,
     `cmd_add_with_keyfile_orders_format_addkey_backup_open`.
   - `replace.rs`, `recover.rs`, `enroll_key_file.rs`: their AddKeyFile/
     HeaderBackup render + order tests.
   - `journal.rs`: `roundtrip_add_recoverable_with_enroll_key_file` and the
     serde-drift guard -- must pass with the literals updated (proving the
     transparent-serde wire format is identical).
2. **Compile-time proof of the invariant** (do not commit): transpose the two
   arguments at one call site and confirm `cargo build` now *fails* with a type
   mismatch -- the swap that previously compiled. Revert.
3. **Required role-mapping coverage** (behavioral + structure-insensitive). The
   newtype guards the *function boundary* (you cannot pass a `HeaderBackupPath`
   where a `KeyFilePath` is expected), but the terminal render still does
   `key_file.as_path().display().to_string()` into a stringly `CmdRequest`
   field by hand -- so an intra-body miswire (keyfile string into
   `HeaderBackup.backup_path`, or the reverse) still compiles. Pin it: at
   **every** render site that emits an addKey/headerBackup pair -- the `add`
   Fresh + returned-disk tests, the two `replace` arms, the two `recover`
   replay arms, and the standalone `enroll` render -- assert *both* exact field
   values with **distinct** keyfile and header paths:
   `AddKeyFile.key_file_path == <keyfile>` **and**
   `HeaderBackup.backup_path == <header path>`. Most existing tests only pin
   order + keyfile presence; extend them (or add one assertion each) so a
   future swap at the stringly boundary fails a test even for code that bypasses
   the types.
4. `just docs-build` -- no doc behavior changes expected; confirms links/ASCII
   checks still pass.
5. End-to-end safety net (unchanged behavior, should pass as-is):
   `tests/cli/braid-add-enroll.py`, `tests/cli/add-enroll-recoverable.py` --
   the slot-1-in-header-backup regression guards.

## Risks / notes

- **Largest blast radius of the three options, but uniform.** Every edit is a
  return-type/field-type change plus `.as_path()`/`.display()` at use sites; no
  control-flow changes.
- **Journal format safety:** `#[serde(transparent)]` over `PathBuf` is the load-
  bearing detail -- it guarantees the on-disk `pending-op.json` shape is
  unchanged, so existing journals replay and the drift-guard test holds.
- **Doc comments:** every new `pub`/`pub(crate)` item (the two `pub` types,
  their `pub fn as_path`/`Display`, and `KeyFilePath::new`) gets a `///` stating
  *why* it exists at that boundary, per AGENTS.md.
- **Visibility:** newtypes are `pub` (sibling convention) with private fields;
  constructors are controlled (`KeyFilePath::new` is `pub(crate)`;
  `HeaderBackupPath` is mintable only inside `luks`). This avoids
  `private_interfaces` warnings from the `pub` `luks` helpers without changing
  their visibility.
- **No `principles.md` / ADR change required:** this strengthens an existing
  invariant's enforcement without changing behavior or adding a new one. If
  desired, a one-line note could be added to the safety-heuristics doc, but it
  is not required.

## Implementation notes

- `luks.rs` does not import `std::fmt`, so `HeaderBackupPath`'s `Display` impl
  is written with the fully-qualified `std::fmt::Display`/`Formatter`/`Result`
  rather than adding a module import (the plan's snippet assumed `fmt::` was in
  scope).
- Test fixtures that previously built `header_backup_path` from a literal
  `PathBuf::from("/tmp/mock-header")` (add.rs `recoverable_target`/`fresh_target`,
  replace.rs `preflight_dispatcher_fresh_luks_is_noop`) now mint it through
  `luks_header_backup_path(...)`, the only constructor -- `HeaderBackupPath` has
  no public ctor by design.
- Test helpers that model journal construction (recover.rs `fresh_mode`,
  `recoverable_pool_mutation_add_journal_with_enroll`, the two replace-journal
  helpers; journal.rs roundtrip literals) keep their `PathBuf`/`Option<PathBuf>`
  author-facing parameters and convert to `KeyFilePath` at the journal-mode
  construction boundary via `.map(KeyFilePath::new)` / `KeyFilePath::new(..)`,
  rather than threading the newtype through every fixture signature.
- Dropped the now-unused bare `PathBuf` import from `add.rs` and `replace.rs`
  (remaining uses are fully-qualified `std::path::PathBuf` in tests); the crate
  `[lints]` table denies unused imports.
- Added one new render test, `recover.rs::render_add_recovery_fresh_luks_with_enroll_pins_keyfile_and_header_fields`:
  the second of the two recover replay arms (FreshLuks) had no existing test
  that rendered the addKey/headerBackup pair (the existing FreshLuks recovery
  render test passes `enroll_key_file: None`), so Verification step 3's
  both-fields-distinct assertion needed a fresh test there.
