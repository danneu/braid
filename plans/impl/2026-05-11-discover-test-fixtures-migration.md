# Plan: Migrate `cli/src/discover.rs` test scaffolding to `test_fixtures::discover`

**Status: Draft**

## Context

`cli/src/discover.rs::mod tests` (lines 304-1024) currently re-defines its
own private mock-runner stack and tempdir helpers: `mock_output`,
`LabelMap` (the workhorse), `create_target`, and `create_by_id_symlink`.
Twelve tests share these helpers, and each test still has to re-instantiate
its own tempdir + symlink structure inline.

The recent `monitor` / `ack` / `idle` / `scrub` migrations (see
`plans/impl/2026-05-09-monitor-test-fixtures-migration.md`,
`plans/impl/2026-05-09-ack-test-fixtures-migration.md`,
`plans/impl/2026-05-11-idle-test-fixtures-migration.md`, and
`plans/impl/2026-05-11-scrub-test-fixtures-migration.md`) lifted similar
per-module scaffolding into `cli/src/test_fixtures/<scope>.rs` modules,
re-exported through the `cli/src/test_fixtures.rs` facade. This plan
applies the same shape to `discover.rs` while preserving the contracts
that make these tests valuable -- especially the realistic command-runner
behavior (`Ok(exit=1)` for unknown devices, never `Err`), the call
recorder that pins the non-LUKS gate, and the real-filesystem symlink /
canonicalize coverage.

This is a test-side refactor only. Do not change `discover_from_dir`,
`DiscoverError`, `DiscoverWarning`, `DiscoverOutcome`, `by_id_priority`,
`is_partition_entry`, `label_collision`, or any other production type or
function in `discover.rs`.

## Goals

Preserve every behavior contract the current tests pin:

- Runner errors at `isLuks` propagate as `DiscoverError::Cmd`, never get
  collapsed into "no labeled disks found".
- Runner errors at `luksDump` propagate as `DiscoverError::Cmd` after
  `isLuks` succeeds; the two command sites are pinned separately.
- Non-LUKS devices never reach `luksDump`: the gate is the `isLuks` exit
  status, not its `Result` variant.
- A braid-labeled disk whose `luksDump` returns non-zero produces a
  structured `DiscoverWarning::LuksDumpFailed { exit_code, path, stderr,
  .. }`, not a silent drop.
- A successful but unparseable `luksDump` stdout produces a distinct
  `DiscoverWarning::LuksDumpUnparseable { path, detail }`. Parser drift
  and header rejection do not collapse into one warning kind.
- The wwn / nvme / scsi / ata / usb / other priority order is enforced
  per physical disk, not globally.
- Same-priority ties break lexicographically by filename, deterministically
  across `read_dir` orderings.
- Best-symlink selection is per physical disk, not shared across disks.
- LUKS1 disks are filtered with `DiscoverWarning::UnsupportedLuksVersion {
  path, version: 1 }` and never enter `members`.
- Invalid `braid-<NAME>` labels produce
  `DiscoverWarning::InvalidDiskName { path, label }` AND are absent from
  `members`. The user-facing `Display` rendering escapes non-ASCII bytes
  via `escape_default()` (e.g. `"braid-\u{e9}"`).
- Two distinct physical devices sharing the same `braid-<name>` label
  produce `DiscoverError::LabelCollision`, with deterministic
  `path1`/`path2` ordering.
- Broken by-id symlinks emit `DiscoverWarning::CannotCanonicalize` and
  are skipped without aborting discovery; remaining valid entries still
  populate `members`.
- `LabelMap`'s realistic runner behavior is preserved verbatim: unknown
  devices return `Ok(RawCommandOutput { exit_status: 1, ... })`, never
  `Err`. The `_ => Err(CmdError::MissingMock)` arm only fires for
  command variants `discover_from_dir` itself never issues.
- Call recording is preserved: `non_luks_device_never_reaches_luks_dump`
  must still be able to assert that no `luksDump` call was made for a
  non-LUKS device path.
- Real filesystem behavior is preserved: tests create real `tempfile`
  directories, real Unix symlinks, and rely on `RealByIdResolver` to
  canonicalize them. Broken-symlink and canonicalize-failure behavior is
  exercised against the real filesystem, not a fake.

## Current-State Inventory

`cli/src/discover.rs` is 1024 lines. The `mod tests` block runs from
line 304 to 1024 and contains 15 tests plus roughly 130 lines of local
scaffolding.

### Local helpers in `discover.rs::tests`

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `mock_output` | 311-318 | Factory for `RawCommandOutput` with empty stderr. Only called from inside `LabelMap`'s `CommandRunner` impl. | Delete in the same commit that promotes `LabelMap`. Replace its in-impl call sites with `super::shared::mock_ok` for exit-0 cases and a small private fixture-local helper for the exit-1 unknown-device branches (see Proposed Fixture Shape). |
| `LabelMap` | 320-414 | Label-driven `CommandRunner` with `with_version`, `with_dump_response`, and a `calls()` recorder. Returns `Ok(exit=1)` for unknown devices on both `CryptsetupIsLuks` and `CryptsetupLuksDumpText`. Used by 10 of the 12 filesystem-dependent tests. | Promote as `DiscoverLabelMap` in `test_fixtures::discover`. Keep the field set, constructor, builder chain, recorder method, and the "unknown device returns Ok(exit=1)" branch behavior unchanged. Rename to add the `Discover` scope prefix (`LabelMap` is generic enough that an un-prefixed name in the facade would be ambiguous and would collide with future label-driven helpers in other domains). |
| `create_target` | 418-422 | Writes an empty placeholder file at `dir/name` and returns its path string. Used by all 12 filesystem-dependent tests. | Promote as `discover_create_target(dir: &Path, name: &str) -> String`. |
| `create_by_id_symlink` | 426-430 | Creates a Unix symlink at `dir/name` pointing at `target`, returns the symlink path string. Used by all 12 filesystem-dependent tests. | Promote as `discover_create_by_id_symlink(dir: &Path, name: &str, target: &str) -> String`. |
| `IsLuksFailRunner` | 444-460 (nested in test) | Strict `CommandRunner` that returns `Err(CmdError::Failed(...))` for every request, including `run_with_stdin`. Used by exactly one test (`discover_propagates_runner_error_at_isluks`). | Keep **local**. The one-test scope and the "fail every request" semantics are part of the assertion: this test is the only one that proves first-call propagation. Promoting it would either weaken the contract (if a sibling test wanted a less strict variant) or duplicate it. Same precedent as `FailingCancelRunner` in the scrub plan. |
| `LuksDumpFailRunner` | 491-516 (nested in test) | Strict `CommandRunner` that returns `Ok(exit=0)` for `CryptsetupIsLuks`, `Err(CmdError::Failed(...))` for `CryptsetupLuksDumpText`, and `Err(CmdError::MissingMock)` for everything else. Used by exactly one test (`discover_propagates_runner_error_at_luksdump`). | Keep **local**, same reasoning. The "second-call failure after a successful first call" shape is the test's setup. |

### Tests in `discover.rs::tests`

Twelve tests do real-filesystem work and use the helpers above; three
are pure unit tests against deterministic helpers.

| # | Test | Lines | Uses LabelMap | Uses tempdir helpers | Asserts on |
|---:|---|---:|:---:|:---:|---|
| 1 | `discover_propagates_runner_error_at_isluks` | 432-478 | -- | yes | `DiscoverError::Cmd(_)` |
| 2 | `discover_propagates_runner_error_at_luksdump` | 480-534 | -- | yes | `DiscoverError::Cmd(_)` |
| 3 | `non_luks_device_never_reaches_luks_dump` | 536-574 | yes (+ `.calls()`) | yes | empty warnings + `luksDump` was only called for the LUKS path |
| 4 | `discover_warns_when_labeled_disk_fails_luksdump` | 576-629 | yes (`with_dump_response`) | yes | `DiscoverWarning::LuksDumpFailed { exit_code: 1, path, stderr, .. }` |
| 5 | `discover_warns_on_unparseable_luksdump_output` | 631-669 | yes (`with_dump_response`) | yes | `DiscoverWarning::LuksDumpUnparseable { path, detail }` |
| 6 | `partition_detection` | 671-678 | -- | -- | `is_partition_entry` truth table |
| 7 | `by_id_priority_ordering` | 680-693 | -- | -- | `by_id_priority` total ordering |
| 8 | `discover_prefers_wwn_over_ata` | 695-726 | yes | yes | discovered member resolves to the wwn- symlink |
| 9 | `discover_same_priority_breaks_ties_lexicographically` | 728-759 | yes | yes | discovered member resolves to the lexicographically earlier ata- symlink |
| 10 | `discover_skips_luks1_disk` | 761-807 | yes (`with_version`) | yes | LUKS2 member present, LUKS1 absent, `UnsupportedLuksVersion { version: 1 }` warning |
| 11 | `discover_warns_on_invalid_disk_name_in_braid_label` | 809-877 | yes | yes | `InvalidDiskName { path, label: "braid-é" }` warning + `Display` escapes `\u{e9}` |
| 12 | `discover_selects_best_symlink_per_disk_independently` | 879-922 | yes | yes | both members resolve to their respective wwn- symlinks |
| 13 | `discover_fails_on_label_collision_across_disks` | 924-961 | yes | yes | `DiscoverError::LabelCollision { name, path1, path2 }` referencing both aliases |
| 14 | `discover_skips_entry_when_canonicalize_fails` | 963-997 | yes | yes (+ broken symlink to `/nonexistent/...`) | valid member kept, `DiscoverWarning::CannotCanonicalize { path }` for the broken alias |
| 15 | `label_collision_sorts_paths_lexicographically` | 999-1023 | -- | -- | `label_collision()` returns lex-ordered paths regardless of input order |

### Behavior families

| Family | Tests | Migration concern |
|---|---|---|
| Runner-layer error propagation | 1, 2 | Keep `IsLuksFailRunner` and `LuksDumpFailRunner` local. Migrate only the tempdir / symlink scaffolding. |
| Non-LUKS gate (call recording) | 3 | `DiscoverLabelMap::calls()` must keep returning `Vec<(String, String)>` with the same insertion order so `.iter().filter(...)` over recorded `luksDump` calls keeps working byte-for-byte. |
| `luksDump` failure warning | 4 | Inline `RawCommandOutput { exit_status: 1, stderr: "not a valid LUKS device" }` body stays inline. The exit code and stderr substring are the assertion's setup; naming this body as a fixture would invite a future test to substitute a canonical body and weaken the proof. |
| `luksDump` parser-drift warning | 5 | Inline `RawCommandOutput { exit_status: 0, stdout: "LUKS header information\nUUID: foo\n" }` body stays inline. The missing-Version-field is what makes the parser fail; the body shape IS the test setup. |
| Symlink priority selection | 8, 9, 12 | Promoted `DiscoverLabelMap` + tempdir helpers. The tests still spell out their by-id entries explicitly so the priority claim stays visible at the call site. |
| LUKS version filtering | 10 | Uses `DiscoverLabelMap::with_version`. No fixture-shape change required. |
| Invalid label rendering | 11 | The `"braid-\\u{e9}"` `escape_default` assertion stays in the test; nothing fixture-side. |
| Hard label collision | 13 | Promoted `DiscoverLabelMap` + tempdir helpers. Two distinct tempdir targets ensure the collision detector sees distinct canonical paths. |
| Broken symlink skipping | 14 | Real broken symlink at `/nonexistent/dangling/target` stays inline; the dangling path is the assertion's setup. `RealByIdResolver` + `canonicalize()` must keep failing on the broken target. |
| Pure unit | 6, 7, 15 | Stay local. No filesystem, no runner, no fixtures. Extracting them would add noise. |

## Existing Fixture Modules

Each candidate for reuse was evaluated against the constraint that the
fixture shape must preserve the exact behavior the discover tests pin.

- **`shared::MockFs`** and other filesystem mocks (`monitor_fs_*`,
  `lock_fs`, `ack_fs_*`, `idle::IdleMockFs`, `unlock::unlock_storage_fs`,
  `mount::mount_fs`, `enroll_key_file::enroll_fs`).
  - Discovery does not read `/proc/self/mountinfo`, sysfs, or any
    `Filesystem`-trait surface. It walks a real on-disk by-id directory
    via `std::fs::read_dir` and resolves symlinks with `std::fs::canonicalize`
    (via `RealByIdResolver`).
  - Replacing the real filesystem with a fake would break the
    canonicalize-failure test (`discover_skips_entry_when_canonicalize_fails`)
    and the priority / tie-break tests, all of which depend on real
    `read_dir` + `canonicalize` semantics.
  - **Decision:** do not use any `MockFs` family. Keep the real tempdir +
    symlink approach.

- **`mount::is_luks_ok` / `mount::is_luks_fail` / `mount::luks_dump_text_ok`
  / `mount::luks_dump_text_fail` / `mount::luks_uuid_ok`.**
  - These are `(CmdRequest, RawCommandOutput)` pair factories tied to
    specific by-id device strings supplied at the call site.
  - Discover's tests don't compose request/response pairs onto a generic
    `MockRunner` -- they install a label-driven runner that answers
    `CryptsetupIsLuks` and `CryptsetupLuksDumpText` for any device in its
    map, with realistic `Ok(exit=1)` fall-through for unknown devices.
    Using `MockRunner::default().with_output(...)` chains would force
    each test to enumerate every probe explicitly, which is the opposite
    of what the call-recording family needs (test 3 asserts on what was
    NOT called, which requires runner-level fall-through, not per-probe
    seeding).
  - **Decision:** do not use any `mount::is_luks_*` / `mount::luks_dump_*`
    helpers. Keep the label-map runner.

- **`shared::mock_ok`.**
  - Pros: it is the canonical `RawCommandOutput { stderr: "",
    exit_status: 0 }` builder, used by every other fixture for exit-0
    bodies.
  - **Decision:** use it **privately** inside `test_fixtures::discover`
    for the exit-0 branches of `DiscoverLabelMap`'s `CommandRunner` impl.
    The exit-1 "unknown device" branches construct `RawCommandOutput`
    inline (or via a tiny private helper -- see Proposed Fixture Shape).
    No facade re-export is required from this migration.

- **`PoolFixture` / `StatePaths` / `RecordingInhibitor`.**
  - Discovery does not touch `pool.json`, `config.json`, the passphrase
    file, or the sleep inhibitor.
  - **Decision:** skip entirely.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/discover.rs` as a flat discover-scoped
module. Register it in `cli/src/test_fixtures.rs` with `mod discover;`
and facade re-exports.

Do not ship a `DiscoverTopology` installer, a "two healthy disks" runner
factory, a params builder, or a by-id tempdir harness struct. Every test
already varies on at least one of (a) which by-id entries exist, (b) which
labels map to which paths, (c) which LUKS version each path reports, (d)
which `luksDump` responses are overridden, and (e) whether targets are
shared across symlinks. A broad scenario installer would hide that surface
at the call site and silently couple unrelated tests through a shared
default.

### Public fixture surface

```rust
// Label-driven mock CommandRunner.
//
// Realistic runner behavior: unknown devices return Ok(exit=1) on both
// CryptsetupIsLuks and CryptsetupLuksDumpText, never Err. The
// `_ => Err(CmdError::MissingMock)` arm only fires for command variants
// `discover_from_dir` does not issue, so a future regression that adds a
// new command to discovery still surfaces as a hard MissingMock.
//
// Records (command_label, device_path) pairs on every isLuks /
// luksDump call. The non-LUKS gate test depends on this recorder.
pub(crate) struct DiscoverLabelMap {
    labels: HashMap<String, String>,
    versions: HashMap<String, u32>,
    dump_responses: HashMap<String, RawCommandOutput>,
    calls: Mutex<Vec<(String, String)>>,
}

impl DiscoverLabelMap {
    /// Build a label map from `(device_path, full_label)` pairs. The
    /// label includes the `braid-` prefix (e.g. `"braid-sda"`), matching
    /// how the local `LabelMap::new` is currently called.
    pub(crate) fn new(entries: &[(&str, &str)]) -> Self;

    /// Override the reported LUKS version for a specific path. Defaults
    /// to 2 (LUKS2) for any path not explicitly set.
    pub(crate) fn with_version(self, path: &str, version: u32) -> Self;

    /// Override the entire luksDump response for a path. Used by tests
    /// that inject realistic exit-1 + stderr or unparseable exit-0
    /// stdout.
    pub(crate) fn with_dump_response(self, path: &str, response: RawCommandOutput) -> Self;

    /// Snapshot the recorded (command_label, device_path) pairs.
    /// Returns owned strings so the caller can drop the runner before
    /// asserting.
    pub(crate) fn calls(&self) -> Vec<(String, String)>;
}

impl CommandRunner for DiscoverLabelMap { /* unchanged dispatch */ }

// Real-filesystem helpers for building a by-id tempdir layout.
//
// Each test owns its own `tempfile::tempdir()` and calls these to
// populate it. The two helpers are intentionally separate (not a
// `ByIdTempdir::with_disk(...)` harness) so each test still spells out
// its targets and aliases at the call site -- the by-id layout IS
// part of what most of these tests prove.
pub(crate) fn discover_create_target(dir: &Path, name: &str) -> String;
pub(crate) fn discover_create_by_id_symlink(dir: &Path, name: &str, target: &str) -> String;
```

### Implementation notes

- All items are `pub(crate)` and `#[cfg(test)]`.
- `DiscoverLabelMap::run`'s exit-0 branches use `super::shared::mock_ok`,
  mirroring the pattern in `idle.rs`, `status.rs`, and `scrub.rs`. The
  two exit-1 unknown-device branches construct `RawCommandOutput` inline
  with `cmd: "cryptsetup".into()`, empty `stderr`, and exit_status: 1.
  The two branches use **different stdout bodies**, copied verbatim from
  the current `LabelMap` so a future regression in either branch is
  still byte-for-byte detectable:
  - Unknown `CryptsetupIsLuks`: stdout empty (matches local
    `discover.rs:376`: `mock_output("cryptsetup", "", 1)`).
  - Unknown `CryptsetupLuksDumpText`: stdout
    `"Device /dev/foo is not a valid LUKS device.\n"` (matches local
    `discover.rs:396-400`:
    `mock_output("cryptsetup", "Device /dev/foo is not a valid LUKS device.\n", 1)`).
    The literal `/dev/foo` device path is intentional in the local
    runner -- it is a stand-in that lets the body parse identically to
    real cryptsetup output without making the fixture aware of the
    real device path under test. Preserving it verbatim keeps the
    promoted runner indistinguishable from the local one.
  There is no need for a private `discover_err_cryptsetup` helper --
  two inline `RawCommandOutput` literals are clearer than a one-off
  named helper that would have to take the stdout body as a parameter.
- `DiscoverLabelMap`'s `run_with_stdin` delegates to `run`, matching the
  current `LabelMap` behavior. Discovery does not issue stdin commands,
  but the delegation keeps the runner usable if `discover_from_dir`'s
  surface ever grows.
- `discover_create_target` calls `std::fs::write(&dir.join(name), b"")`
  and returns the path via `to_string_lossy().into_owned()`. Identical
  to the local helper, only the name changes.
- `discover_create_by_id_symlink` calls
  `std::os::unix::fs::symlink(target, &dir.join(name))` and returns the
  symlink path via `to_string_lossy().into_owned()`. Identical to the
  local helper, only the name changes.
- Both helpers take `&Path` (not `&Path` wrapped in any temp-handle type)
  so callers can pass `tempdir.path()` directly without going through a
  fixture-specific tempdir adapter.
- The `Discover` scope prefix on `DiscoverLabelMap` and `discover_` on
  the function helpers serves two purposes: (1) a generic name like
  `LabelMap` in the test_fixtures facade would clash with potential
  future label-driven helpers in other domains; (2) the prefix lets the
  staged migration import a fixture helper while the same-purpose local
  still exists for unmigrated tests (see Staged Migration). Unlike the
  scrub plan, this migration explicitly relies on the prefix for staging
  safety -- the local names (`LabelMap`, `create_target`,
  `create_by_id_symlink`) are not already prefixed.

### What stays local in `discover.rs::tests`

- `IsLuksFailRunner` (currently 444-460). Used by exactly one test
  (`discover_propagates_runner_error_at_isluks`). Its "fail every
  request, including `run_with_stdin`" shape is the test's load-bearing
  setup. Promoting it would either weaken that contract or duplicate it.
- `LuksDumpFailRunner` (currently 491-516). Used by exactly one test
  (`discover_propagates_runner_error_at_luksdump`). The "isLuks Ok, then
  luksDump fails, everything else is MissingMock" shape is the test's
  setup; the per-request match arms are the assertion's prelude.
- The inline `RawCommandOutput` body in
  `discover_warns_when_labeled_disk_fails_luksdump` (exit_status: 1,
  stderr: `"Device /dev/foo is not a valid LUKS device.\n"`). The
  specific stderr substring is what `outcome.warnings[0]` asserts on; a
  named factory would invite a future test to substitute a canonical
  body and weaken the proof.
- The inline `RawCommandOutput` body in
  `discover_warns_on_unparseable_luksdump_output` (exit_status: 0,
  stdout: `"LUKS header information\nUUID: foo\n"`). The missing
  `Version` field is the parser-failure trigger; the body shape IS the
  assertion's setup, and `detail.contains("Version")` ties the assertion
  to that exact body.
- The three pure unit tests: `partition_detection` (671-678),
  `by_id_priority_ordering` (680-693),
  `label_collision_sorts_paths_lexicographically` (999-1023). They use
  no runner, no filesystem, no helpers. Fixture extraction would add
  noise.
- The per-test `/* Intent / Why it exists / Scenario */` preambles. The
  fixture module is for reusable bodies, not test prose.

### Facade exports

Add a discover block to `cli/src/test_fixtures.rs` next to the existing
modules:

```rust
mod discover;

#[allow(unused_imports)]
pub(crate) use discover::{
    DiscoverLabelMap, discover_create_by_id_symlink, discover_create_target,
};
```

Update the module-level comment in `cli/src/test_fixtures.rs` with one
discover bullet:

> `discover` -- flat discover-shaped helpers: `DiscoverLabelMap`
> (label-driven mock runner with realistic `Ok(exit=1)` fall-through for
> unknown devices and a call recorder for the non-LUKS gate test),
> `discover_create_target`, and `discover_create_by_id_symlink` (real
> tempdir / Unix symlink builders). Ships flat because per-test by-id
> entries, labels, versions, and dump responses are load-bearing claims
> at the call site; a broad "two healthy disks" runner would hide them.
> The `Discover` / `discover_` prefix avoids facade collisions with
> other fixture families and lets the staged migration import a fixture
> helper while the same-purpose local still exists.

## Staged Migration

Three sub-commits. Each compiles and keeps

```sh
cargo test --manifest-path cli/Cargo.toml --lib discover::tests
just test-rust
```

green at every boundary. The `Discover` / `discover_` prefix on the
promoted names lets locals and fixtures coexist between commits, so each
commit migrates the call sites for one helper family at a time and
deletes the corresponding locals in the same commit.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(discover): add discover fixture module` | Add `cli/src/test_fixtures/discover.rs` with `DiscoverLabelMap`, `discover_create_target`, `discover_create_by_id_symlink`. Register `mod discover;` and the three facade re-exports in `cli/src/test_fixtures.rs`. Update the `test_fixtures.rs` module doc comment with the new `discover` bullet. No `discover.rs` call sites change yet; no locals are deleted yet. The local `LabelMap`, `mock_output`, `create_target`, and `create_by_id_symlink` keep compiling and all 15 tests still call the local versions. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib discover::tests`; `just test-rust` |
| 2 | `test(discover): migrate label-map tests to DiscoverLabelMap` | In `discover.rs::tests`: import `DiscoverLabelMap` from the facade. Replace every `LabelMap::new(...)` call site (tests 3, 4, 5, 8, 9, 10, 11, 12, 13, 14 -- 10 tests) with `DiscoverLabelMap::new(...)`. The `.with_version`, `.with_dump_response`, and `.calls()` call sites are unchanged because the new struct has the same builder chain and recorder method. Delete local `LabelMap` (the struct, both impls) and `mock_output` (now unused since `LabelMap` was its only caller) in the same commit. Keep local `create_target`, `create_by_id_symlink`, `IsLuksFailRunner`, and `LuksDumpFailRunner` in place -- tests 1 and 2 still rely on them, and the 10 migrated tests still call the local tempdir helpers. | `cargo check --manifest-path cli/Cargo.toml --tests`; run all ten migrated `discover::tests::*` by name; `cargo test --manifest-path cli/Cargo.toml --lib discover::tests`; `just test-rust` |
| 3 | `test(discover): migrate tempdir helpers to fixture module` | In `discover.rs::tests`: import `discover_create_target` and `discover_create_by_id_symlink` from the facade. Replace every `create_target(...)` and `create_by_id_symlink(...)` call site across all 12 filesystem-dependent tests (tests 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14). Delete local `create_target` and `create_by_id_symlink` in the same commit. `IsLuksFailRunner` and `LuksDumpFailRunner` stay local -- they don't reference the deleted helpers. The three pure-unit tests (6, 7, 15) are untouched. | `cargo check --manifest-path cli/Cargo.toml --tests`; run all twelve filesystem-dependent `discover::tests::*` by name; `cargo test --manifest-path cli/Cargo.toml --lib discover::tests`; `just test-rust` |

After sub-commit 3, `discover.rs::tests` contains only:

- The 12 filesystem-dependent tests, now using fixture helpers, with
  their inline `RawCommandOutput` bodies (tests 4 and 5) preserved.
- The 3 pure unit tests, untouched.
- `IsLuksFailRunner` and `LuksDumpFailRunner` as nested structs inside
  their owning tests.

No separate cleanup commit is needed: every migration sub-commit deletes
the locals it obsoletes in the same commit, so the module ends each
sub-commit with no dead code and no orphaned imports. Run
`cargo check --manifest-path cli/Cargo.toml --tests` as part of every
sub-commit boundary to catch dead `use` imports or stranded helpers.

## Risks

- **Hiding the realistic `Ok(exit=1)` fall-through.** If a future
  migration replaces `DiscoverLabelMap`'s "unknown device returns
  Ok(exit=1)" branches with `Err(MissingMock)`, the non-LUKS gate test
  (test 3) would still pass for the wrong reason (the gate would short
  circuit on the error result, not on the exit status), and a real
  regression that re-checked `.is_err()` instead of `.exit_status() == 0`
  would slip through. Mitigation: the fixture's `run` body keeps the
  exact branch structure of the local `LabelMap` -- explicit `Ok(...)`
  for both known and unknown devices, `Err(MissingMock)` only for
  command variants discovery never issues. The plan calls this out in
  the helper doc comment so a future contributor doesn't "simplify" it.
- **Losing the call recorder's insertion-order semantics.** Test 3
  (`non_luks_device_never_reaches_luks_dump`) currently does
  `runner.calls().into_iter().filter(...)` and asserts all surviving
  `luksDump` calls reference the LUKS path. The assertion does not
  depend on the absolute order of `isLuks` vs `luksDump`, but it does
  depend on the recorder being a complete log of every (command, device)
  attempt. Mitigation: `DiscoverLabelMap`'s `Mutex<Vec<(String,
  String)>>` recorder is copied byte-for-byte from `LabelMap`; the
  `push` call site stays inside the matching `match` arms so order is
  preserved.
- **Weakening the runner-layer error contract by promoting
  `IsLuksFailRunner` / `LuksDumpFailRunner`.** A shared "is_luks_fail" or
  "luks_dump_fail" fixture would either need to choose a single
  `CmdError` variant (locking the test to a body that diverges from real
  spawn failures), or accept a parameter (re-inviting the original
  scaffolding into the fixture module without saving anything). It would
  also need a stance on `run_with_stdin` (currently both runners delegate
  to `run`). Mitigation: keep both runners local. Each is one test's
  setup. The scrub plan made the same call for `FailingCancelRunner`.
- **Substituting an "unparseable luksDump" or "luksDump failed" fixture
  for the inline bodies in tests 4 and 5.** Naming those bodies as
  `discover_luksdump_invalid_device_stderr` or
  `discover_luksdump_missing_version_stdout` would imply the body is
  reusable, and a future test could mistakenly use the canonical body
  while expecting different assertions. Mitigation: the plan keeps both
  bodies inline. The exit code + stderr (test 4) and the missing-Version
  stdout (test 5) ARE the assertion setup; they are not reusable across
  tests with different warning shapes.
- **Real-filesystem coverage regression from a `MockFs` shortcut.**
  Discovery exercises real `read_dir`, real symlinks, real
  `canonicalize`, and real broken-symlink failures (test 14). Swapping
  the tempdir helpers for a `Filesystem`-trait mock would lose the
  canonicalize-failure proof and the priority/tie-break tests would no
  longer reflect what the kernel does on `/dev/disk/by-id`. Mitigation:
  the promoted helpers are byte-for-byte the same `std::fs::write` +
  `std::os::unix::fs::symlink` calls. The plan explicitly does NOT
  introduce a `Filesystem`-trait fake for this module.
- **Facade churn vs. existing modules.** Adding three new re-exports is
  small surface change. Mitigation: gate them with
  `#[allow(unused_imports)]`, group them on one `pub(crate) use` line,
  and add a single bullet to the module doc comment.
- **Overprescribing the test structure.** The implementation may choose
  to leave each filesystem-dependent test exactly as-is (only the
  identifiers change) or to consolidate some setup calls into a small
  per-test local helper. The plan requires preserving behavior and the
  realistic runner contract, not a specific assertion or setup layout.

## Verification

Use filtered Rust tests during each sub-commit, then the full module
test, then `just test-rust`:

```sh
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib discover::tests
just test-rust
```

Run `cargo check --manifest-path cli/Cargo.toml --tests` at every
sub-commit boundary -- not only after adding the fixture module
(sub-commit 1) but also after each migration sub-commit (2 and 3) --
because each migration sub-commit deletes locals in-place and must leave
the module free of unused imports, dead references, and facade wiring
errors.

The behavior-pin tests to run by name at sub-commit 2 (LabelMap
migration):

```sh
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::non_luks_device_never_reaches_luks_dump
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_warns_when_labeled_disk_fails_luksdump
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_warns_on_unparseable_luksdump_output
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_prefers_wwn_over_ata
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_same_priority_breaks_ties_lexicographically
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_skips_luks1_disk
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_warns_on_invalid_disk_name_in_braid_label
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_selects_best_symlink_per_disk_independently
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_fails_on_label_collision_across_disks
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_skips_entry_when_canonicalize_fails
```

The additional tests to run by name at sub-commit 3 (tempdir migration):

```sh
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_propagates_runner_error_at_isluks
cargo test --manifest-path cli/Cargo.toml --lib discover::tests::discover_propagates_runner_error_at_luksdump
```

No VM fixture capture is required. This migration does not change
parser fixtures, nixpkgs inputs, or production parser behavior.
