# Plan: Migrate `cli/src/enroll_key_file.rs` test scaffolding to a shared `test_fixtures::enroll_key_file` module

**Status: Draft**

## Context

`cli/src/enroll_key_file.rs` is 3099 lines, of which lines 685-3099 (the `#[cfg(test)] mod tests` block) hold 47 tests plus ~225 lines of inline scaffolding: a local `MockFs` (685:700), `test_paths` (694), `by_id` (730), `passphrase` (734), `ok_raw` / `err_raw` (738/747), `mountpoint_ok` / `mountpoint_fail` (756/766), `with_mountpoint_ok` / `with_mountpoint_fail` (776/781), `make_membership` (786), `luks_uuid_ok` / `luks_uuid_not_luks` (796/808), `luks_dump_slot1_empty` / `luks_dump_slot1_occupied` (821/833), `test_passphrase_ok` / `test_passphrase_fail` (845/858), `test_keyfile_ok` / `test_keyfile_fail` (875/885), `enroll_ok` (895), `make_existing_keyfile` (1193), `discovery_two_disks` (1203).

The 47 tests cluster into nine families:

- **discovery** (6, lines 926-1192) -- `plan_enroll` discovery: present/absent/non-LUKS, skip-note preservation in success and `Err`, bracketed-vs-plain stderr render.
- **generate target validation** (5, 1223-1413) -- `--generate` rejects missing dir / non-dir / non-mountpoint / pre-existing `braid.key` before LUKS discovery; dry-run target-validation has no side effect.
- **existing-keyfile validation** (3, 1414-1477) -- `validate_key_file_path`: regular file accepted without mountpoint; short file rejected; non-generate plan never issues `MountpointCheck`.
- **dry-run probe** (5, 1486-1773) -- `plan_enroll` dry-run idempotency, `emit_status` row ordering for the keyfile probe loop, `--generate --dry-run` skips the probe, probe-error preserves accumulated notes, real-run skips dry-run probe.
- **plan_enrollment / plan_single_disk** (16, 1825-2628) -- batched passphrase verify, all-need / all-already / mixed, `emit_status` row emission, wrong-passphrase wording, slot-1 conflict, single-disk planning helper, `OpenFailed{exit:5}` regression probe, `GenerateNew` skips probe / does not double-verify / still detects slot-1 conflict, divergent-passphrase aborts before probe in both modes.
- **apply_enrollment** (4, 2638-2826) -- `NeedsEnroll` enrolls and backs up, `AlreadyEnrolled` skipped (zero requests), mixed plan only mutates `NeedsEnroll`, post-enroll backup failure surfaces enriched remediation error.
- **generate side effects** (3, 2836-2880) -- `generate_key_file` rejects existing, creates 4096 bytes mode `0o400`, `create_new(true)` blocks TOCTOU.
- **cmd-level integration** (3, 2890-3043) -- recovery-mode gate fires before any cryptsetup; `--generate` with wrong passphrase does NOT create the keyfile; `--generate --dry-run` short-circuits before passphrase / probe / generation.
- **dry-run rendering** (2, 3047-3098) -- `compile_enroll_steps` line counts for `--generate` 3-disk and existing-keyfile 2-disk shapes (no runner, no fs).

Eleven of these 47 tests carry load-bearing invariants the migration must preserve byte-for-byte:

- **Exact `runner.requests()` ordering** -- `plan_all_already_enrolled` (line 1923) and `plan_divergent_passphrase_existing_keyfile_errors_on_disk2` (line 2557) both `assert_eq!(runner.requests(), vec![...])`. Any topology that introduces a `with_handler` shadow or reshuffles seeded requests breaks them.
- **Request counts** -- `plan_generate_new_does_not_repeat_first_candidate_passphrase_verify` (2412), `dry_run_with_generate_skips_probe` (1660), `real_run_does_not_probe_before_passphrase` (1775), and the equivalent inside `non_generate_plan_does_not_require_mountpoint` (1414) count `iter().filter(...)` matches.
- **Deliberate `MissingMock` probes** -- `plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds` (2254, no `LuksDump` mock), `plan_generate_new_skips_keyfile_probe` (2318, no `TestKeyFile` mocks), `plan_divergent_passphrase_generate_new_errors_on_disk2` (2586, no `LuksDump` for d2), `apply_skips_already_enrolled` (2707, default runner), `cmd_enroll_blocked_in_recovery_mode` (2890, default runner), `cmd_generate_dry_run_short_circuits` (3005, no passphrase / probe / dump mocks). Any topology that auto-resolves these probes silently inverts the assertion.
- **`with_output_stdin` byte-string invariants** -- ~20 sites pin exact passphrase bytes for `CryptsetupTestPassphrase` and `CryptsetupLuksAddKeyFile`. Tests vary the passphrase per scenario (`"testpass"`, `"wrongpass"`); there is no single canonical value to share.
- **Real filesystem side-effect assertions** -- `apply_enrolls_needs_enroll_items` (2638), `apply_mixed_plan` (2731), `apply_enrollment_returns_enriched_error_when_backup_fails` (2787) assert `paths.luks_headers_dir().join(...).exists()` post-call; `cmd_generate_wrong_passphrase_no_keyfile_created` (2945) and `cmd_generate_dry_run_short_circuits` (3005) assert `!kf.exists()`; `generate_key_file_creates_4096_bytes_mode_400` (2851) inspects file metadata. Real `tempfile::TempDir` and real `std::fs` calls must stay real.

Outcome: ship `cli/src/test_fixtures/enroll_key_file.rs` as a flat collection of helpers (modeled on `test_fixtures/mount.rs` and `test_fixtures/doctor.rs` -- no `*Pool` topology installer, no `*ParamsBuilder`). The test surface is dominated by per-test request-set composition and load-bearing missing-mock contracts; the only meaningful composite that already exists -- `discovery_two_disks` -- gets promoted as-is. Reuse `shared::MockFs::unmounted` for the `Filesystem` mock (verified safe by the same `fs.read_to_string` / `fs.is_block_device` / `fs.list_dir` audit that mount used; enroll's call graph adds `enroll_key_file.rs` itself to that audit -- grep is in Verification). Reuse `shared::mock_ok` for the `(cmd, stdout)` factory (byte-identical to the local `ok_raw`). Reuse `mount::err_raw` for the `(cmd, exit_code, stderr)` factory (already exported via the facade at `test_fixtures.rs:66`; byte-identical to the local `err_raw`). Reuse `doctor::isolated_paths` for the `(TempDir, StatePaths)` pair (byte-identical to the local `test_paths`; already re-exported through the facade at `test_fixtures.rs:61`). Every newly-exported helper carries an `enroll_` prefix (`enroll_fs`, `enroll_by_id`, `enroll_luks_uuid_ok`, ...). The prefix is load-bearing: (a) it sidesteps four genuine collisions with helpers already exported through the facade today -- `mountpoint_ok` / `mountpoint_fail` from `doctor` (`test_fixtures.rs:61`) and `err_raw` / `luks_uuid_ok` / `test_passphrase_fail` from `mount` (`test_fixtures.rs:66-68`), the latter three with deliberately different signatures (mount returns pairs while enroll returns triples that bundle stdin); (b) it lets the staged migration import a fixture helper into the test mod while the same-purpose local function still exists for unmigrated tests, since Rust treats a `use` and a same-named local `fn` in the same module as duplicate definitions. Migrate tests in five small sub-commits keeping `just test-rust` green at each boundary.

This is unreleased software (AGENTS.md "No backwards compatibility"), so we delete old scaffolding rather than deprecate it.

## Recommended approach

### A. New module `cli/src/test_fixtures/enroll_key_file.rs`

Gated `#[cfg(test)]`; registered in `cli/src/test_fixtures.rs` as a private submodule (`mod enroll_key_file;`) with `#[allow(unused_imports)] pub(crate) use enroll_key_file::{...}` re-exports through the facade -- matching the existing pattern at `test_fixtures.rs:47-82`. Sibling test code imports via the facade only, e.g. `use crate::test_fixtures::{enroll_fs, enroll_by_id, enroll_passphrase, enroll_luks_uuid_ok, mock_ok, isolated_paths, ...}; use crate::test_fixtures::err_raw as enroll_err_raw;` -- never `crate::test_fixtures::enroll_key_file::{...}`, since `mod enroll_key_file;` is private to `test_fixtures.rs`. The `err_raw` reuse is mandatory-aliased on import (see "Reused via existing facade exports" below for why); the other reuses (`mock_ok`, `isolated_paths`) are imported bare. All items inside the new module are `pub(crate)` and test-only. **Naming convention: every newly-exported helper carries an `enroll_` prefix.** This is enforced by two distinct pressures: (1) the facade already exports `mountpoint_ok` / `mountpoint_fail` from `doctor` and `err_raw` / `luks_uuid_ok` / `test_passphrase_fail` / `ok_raw` from `mount` (`test_fixtures.rs:61, 66-68`); unprefixed enroll helpers with the same names would collide at the facade. (2) During the staged migration (sub-commits 2-4), each migrated test's `use crate::test_fixtures::...;` line lands while the same-named local helpers still exist in `enroll_key_file.rs::tests` for tests that have not yet been migrated -- and Rust treats `use foo::bar;` plus a same-named local `fn bar` in the same module as a duplicate-definition error. Prefixing the imports avoids both collisions and is the convention sub-commit 5 keeps after the locals are deleted. Module-level doc comment explains why this scope ships flat helpers (no topology installer, no params builder) -- the load-bearing-`MissingMock` constraint plus the per-test request-set diversity -- and documents the `enroll_` prefix decision so a future reviewer doesn't try to "simplify" by stripping the prefix.

Items in the module:

```rust
// Filesystem
pub(crate) fn enroll_fs(paths: &[&str]) -> shared::MockFs;
    // Thin wrapper: shared::MockFs::unmounted(paths.iter().map(...).collect()).
    // Centralises the &str -> String conversion. Safe to use the shared
    // "unmounted" mountinfo body because enroll_key_file's call graph
    // (enroll_key_file.rs, probe.rs, luks.rs, credential_verify.rs) never
    // calls fs.read_to_string, fs.is_block_device, or fs.list_dir -- only
    // fs.exists. Verified by grep -- see Verification.

// Identifier / credential primitives
pub(crate) fn enroll_by_id(path: &str) -> ByIdPath;
pub(crate) fn enroll_passphrase(s: &str) -> Passphrase;
    // Wraps Passphrase::from_zeroizing(zeroize::Zeroizing::new(s.into())).

// (CmdRequest, RawCommandOutput) pair factories for chaining onto MockRunner.
// All return the JSON / canonical-UUID shapes enroll tests rely on -- distinct
// from mount's same-keyword helpers, hence the prefix.
pub(crate) fn enroll_luks_uuid_ok(device: &str) -> (CmdRequest, RawCommandOutput);
    // Returns the canonical aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee UUID for
    // every device. Distinct from mount::luks_uuid_ok which takes a uuid
    // arg -- enroll tests never assert on the UUID value, only that the
    // probe succeeds. Even if the signatures matched, the prefix is still
    // required to keep both helpers reachable through the same facade.
pub(crate) fn enroll_luks_uuid_not_luks(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn enroll_luks_dump_slot1_empty(device: &str) -> (CmdRequest, RawCommandOutput);
    // Returns CmdRequest::CryptsetupLuksDump (JSON variant) with
    // r#"{"keyslots":{"0":{"type":"luks2"}}}"# -- distinct from mount's
    // CryptsetupLuksDumpText (text variant). Slot-1 dumps drive
    // check_key_slot's empty/occupied branch.
pub(crate) fn enroll_luks_dump_slot1_occupied(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn enroll_test_keyfile_ok(device: &str, key_file: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn enroll_test_keyfile_fail(device: &str, key_file: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn enroll_mountpoint_ok(dir: &Path) -> (CmdRequest, RawCommandOutput);
    // Takes &Path (the per-test target dir, e.g. /mnt/usb), not the
    // canonical /mnt/storage that doctor::mountpoint_ok hardcodes -- so
    // doctor's helper is not a drop-in. Direct collision with the
    // already-exported `mountpoint_ok` from doctor at test_fixtures.rs:61
    // is the load-bearing reason for the `enroll_` prefix on this name.
pub(crate) fn enroll_mountpoint_fail(dir: &Path) -> (CmdRequest, RawCommandOutput);

// (CmdRequest, Vec<u8>, RawCommandOutput) triple factories for with_output_stdin.
// Triple shape -- not pair -- because enroll tests vary the passphrase bytes
// per scenario (testpass, wrongpass, etc.) and ~20 call sites already
// destructure into (req, stdin, out). Distinct from mount's pair-shaped
// `test_passphrase_fail` which pairs with the MOUNT_TEST_PASSPHRASE_BYTES
// constant at call sites.
pub(crate) fn enroll_test_passphrase_ok(
    device: &str, passphrase: &str
) -> (CmdRequest, Vec<u8>, RawCommandOutput);
pub(crate) fn enroll_test_passphrase_fail(
    device: &str, passphrase: &str
) -> (CmdRequest, Vec<u8>, RawCommandOutput);
pub(crate) fn enroll_add_keyfile_ok(
    device: &str, key_file: &str, passphrase: &str
) -> (CmdRequest, Vec<u8>, RawCommandOutput);
    // CmdRequest::CryptsetupLuksAddKeyFile { device, key_file_path } with
    // passphrase bytes as stdin and an ok output. Used by every apply_*
    // test. Renamed from the local `enroll_ok` to make the cryptsetup
    // operation explicit (the local name reads as "enroll succeeded" but
    // the helper actually mocks `cryptsetup luksAddKey`).

// Runner-chaining wrappers
pub(crate) fn enroll_with_mountpoint_ok(runner: MockRunner, dir: &Path) -> MockRunner;
pub(crate) fn enroll_with_mountpoint_fail(runner: MockRunner, dir: &Path) -> MockRunner;

// Membership / keyfile composers
pub(crate) fn enroll_make_membership(disks: &[(&str, &str)]) -> PoolMembership;
    // BTreeMap-of-DiskMember from (name, by-id-path) pairs. Distinct from
    // mount::two_disk_membership / three_disk_membership which hardcode
    // virtio-diskN; enroll tests use short by-id strings (d1, d2) and
    // arbitrary disk names so a parameterised builder is the right shape.
pub(crate) fn enroll_make_existing_keyfile(tmp: &TempDir) -> (PathBuf, String);
    // Writes KEYFILE_SIZE bytes to <tmp>/braid.key. Returns (path, str)
    // because most call sites need the display string for
    // CryptsetupTestKeyFile { key_file_path, .. } seeding.

// Composite preflight runner (the only meaningful composite worth keeping)
pub(crate) fn enroll_discovery_two_disks(d1: &str, d2: &str) -> MockRunner;
    // Identical to enroll_key_file.rs:1203 today: enroll_luks_uuid_ok x2,
    // with_luks_dump_text_luks2_for(&[d1, d2]), mappers
    // braid-disk1+braid-disk2 closed. Used by the dry-run probe family
    // (5 tests) and the existing-keyfile validation family (1 test) --
    // 6 known consumers justify promoting unchanged.
```

**Reused via existing facade exports (no new declarations):**

- `err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput` -- byte-identical to enroll's local `err_raw`. Already exported through `pub(crate) use mount::{... err_raw ...}` at `test_fixtures.rs:66`. Migrated tests **import via alias**: `use crate::test_fixtures::err_raw as enroll_err_raw;` and rewrite call sites to `enroll_err_raw(...)`. The alias is required, not cosmetic: the local `fn err_raw` in `enroll_key_file.rs::tests` (line 747) survives sub-commits 2-4 for unmigrated tests, and `use crate::test_fixtures::err_raw;` plus the local `fn err_raw` in the same module is a duplicate-definition error. The alias gives the same call-site shape as the rest of the migration's `enroll_*`-prefixed factories. Sub-commit 5 deletes the local; the alias **stays** because flipping migrated call sites back from `enroll_err_raw(...)` to `err_raw(...)` is unnecessary churn and would break the module-wide `grep enroll_` audit.
- `mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput` -- byte-identical to enroll's local `ok_raw`. Already exported through `pub(crate) use shared::{... mock_ok}` at `test_fixtures.rs:82`. Migrated tests `use crate::test_fixtures::mock_ok;` and call `mock_ok(...)` where the local previously called `ok_raw(...)`. **No alias needed**: the local helper is named `ok_raw`, not `mock_ok`, so the bare import does not collide.
- `isolated_paths() -> (TempDir, StatePaths)` -- byte-identical to enroll's local `test_paths`. Already exported through `pub(crate) use doctor::{... isolated_paths ...}` at `test_fixtures.rs:61`. Migrated tests `use crate::test_fixtures::isolated_paths;` and call `isolated_paths()` where the local previously called `test_paths()`. **No alias needed**: the local helper is named `test_paths`, not `isolated_paths`.

**What does NOT go in this module** (intentional omissions):

- **No `EnrollPool` / `EnrollTopology` handler installer.** Eleven tests pin exact request orderings, request counts, or deliberate missing-mock contracts (Context). A broad `with_handler` would either resolve a probe an `MissingMock` test wants to fail (silent inversion) or shadow seeded requests in ways that shift `runner.requests()` indices (silent breakage of the `assert_eq!` tests). Mirrors `mount.rs`'s decision (`test_fixtures/mount.rs:11-20`) and `recover.rs`'s (`test_fixtures/recover.rs:1-14`).
- **No `EnrollKeyFileParamsBuilder`.** Only 3 of the 47 tests construct an `EnrollKeyFileParams` (`cmd_enroll_blocked_in_recovery_mode` 2890, `cmd_generate_wrong_passphrase_no_keyfile_created` 2945, `cmd_generate_dry_run_short_circuits` 3005). They are heterogeneous (recovery-mode gate / wrong-passphrase abort / dry-run short-circuit) and configure different fields per scenario. A builder for three call sites would add boilerplate without saving lines, and `add` / `replace` already ship their own builders that take an `enroll_key_file: Option<&Path>` setter.
- **No `PoolFixture`.** Enroll never reads `pool.json` from disk. It takes `&PoolMembership` directly via `EnrollKeyFileParams::membership`. `make_membership` is the right shape; `PoolFixture` would write a `pool.json` that never gets read.
- **No `base_two_disk_planner_runner`.** Tempting analogue of `base_two_disk_runner` from mount, but enroll tests vary the per-disk outcome combinatorially (verify_pass / verify_fail x keyfile_ok / keyfile_fail x slot1_empty / slot1_occupied) and -- critically -- the `CryptsetupTestKeyFile { device, key_file_path }` request key includes the keyfile path, which differs per test (`/tmp/braid.key`, `/mnt/usb/braid.key`, the `make_existing_keyfile` tempdir-derived path). Per-test `with_output_stdin` overrides on top of a base would have to match the same compound key, which is fragile. Cleaner to keep the leaf factories and let each test compose explicitly.
- **No new `Filesystem` trait impl.** `shared::MockFs::unmounted` already implements `Filesystem` and matches enroll tests' needs once we confirm production code never calls `fs.read_to_string` / `fs.is_block_device` / `fs.list_dir` from the call graph (verification grep below).
- **No promotion of `apply_*` / `generate_key_file_*` real-fs tests into the fixture.** `apply_enrolls_needs_enroll_items`, `apply_mixed_plan`, `apply_enrollment_returns_enriched_error_when_backup_fails`, `generate_key_file_creates_4096_bytes_mode_400`, `generate_rejects_existing_keyfile`, `generate_key_file_create_new_rejects_existing` rely on real `tempfile::TempDir` + `std::fs` reads/writes for their assertions (file count, mode, existence-after-failure). They use the leaf factories from the fixture for runner seeding but their tempdir + `std::fs::write(...)` lines stay inline.
- **No promotion of the two pure-render tests.** `dry_run_render_enroll_generate_3_disks` (3051) and `dry_run_render_enroll_existing_keyfile` (3080) call `compile_enroll_steps` + `Step::render_dry_run` directly; no runner or fs is involved. Migration is a one-line `use` swap for `by_id` / `isolated_paths` -- nothing to compose.

### B. Migration ordering principle

Move scaffolding once, then replace local references one family at a time. Hard cases first, bulk second:

- (a) **Load-bearing-invariant tests** are the highest-risk family because they pin exact `runner.requests()` orderings, exact request counts, or load-bearing missing-mock contracts (Context's eleven tests). Migration is import-only -- the leaf factories are byte-identical to today's locals, no new dispatch path is introduced. Land first to validate the swap doesn't shift request logs.
- (b) **discovery + dry-run probe + existing-keyfile validation tests** consume `discovery_two_disks` and the `with_mountpoint_*` helpers. Bulk import-only swap.
- (c) **generate-target-validation tests** consume `with_mountpoint_*` and `make_existing_keyfile`. Bulk import-only swap.
- (d) **plan_enrollment / plan_single_disk / apply / cmd-level / pure-render tests** are the long tail -- import-only swaps for the leaf factories (`luks_uuid_ok`, `luks_dump_slot1_*`, `test_passphrase_*`, `test_keyfile_*`, `enroll_ok`, `by_id`, `passphrase`, `make_membership`).
- (e) **Cleanup**: delete the now-unused locals in one commit.

### C. Migration table

| Sub-commit | Action | Validates |
|---|---|---|
| 1 | Land `cli/src/test_fixtures/enroll_key_file.rs` with the items in §A (every newly-exported name carries the `enroll_` prefix). Register `mod enroll_key_file;` (private) + `#[allow(unused_imports)] pub(crate) use enroll_key_file::{...}` facade re-exports in `test_fixtures.rs` (matching the existing groups at lines 47-82 -- the `unused_imports` allow is necessary because consumer migration spans sub-commits 2-4; without it sub-commit 1 fails `cargo check --tests` on the unconsumed re-exports). Mark every item in the new module `#[allow(dead_code)]` since no consumers yet. Do **not** add a new `err_raw` re-export -- mount's existing `err_raw` (`test_fixtures.rs:66`) is byte-identical and already in the facade; consumers in sub-commits 2-4 import it via the alias `use crate::test_fixtures::err_raw as enroll_err_raw;` (the bare `use crate::test_fixtures::err_raw;` collides with the still-extant local `fn err_raw` at `enroll_key_file.rs:747`; see "Reused via existing facade exports" in §A). For `mock_ok` (already at `test_fixtures.rs:82`) and `isolated_paths` (already at `test_fixtures.rs:61`), no alias is needed -- the corresponding locals are named `ok_raw` / `test_paths` and don't collide -- so consumers `use crate::test_fixtures::mock_ok;` and `use crate::test_fixtures::isolated_paths;` bare. **Verify** that `enroll_key_file.rs`, `probe.rs`, `luks.rs`, `credential_verify.rs` never call `fs.read_to_string`, `fs.is_block_device`, or `fs.list_dir` (grep cited in Verification). Update `test_fixtures.rs` module-level doc comment to mention the new scope and to record the `enroll_` prefix decision (including the `err_raw as enroll_err_raw` alias rationale). | Module compiles; `cargo check --manifest-path cli/Cargo.toml --tests` clean; `just test-rust` green. |
| 2 | **Load-bearing-invariant family -- import-only migration.** Replace local helper references with `use crate::test_fixtures::{...}` imports (facade re-exports, never `crate::test_fixtures::enroll_key_file::{...}`) for the 11 highest-risk tests: `plan_all_already_enrolled` (1882), `plan_divergent_passphrase_existing_keyfile_errors_on_disk2` (2515), `plan_generate_new_does_not_repeat_first_candidate_passphrase_verify` (2380), `plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds` (2253), `plan_generate_new_skips_keyfile_probe` (2317), `plan_divergent_passphrase_generate_new_errors_on_disk2` (2585), `apply_skips_already_enrolled` (2706), `cmd_enroll_blocked_in_recovery_mode` (2889), `cmd_generate_dry_run_short_circuits` (3004), `dry_run_with_generate_skips_probe` (1659), `real_run_does_not_probe_before_passphrase` (1774). Per-test `use` lines pull `enroll_by_id`, `enroll_passphrase`, `enroll_make_membership`, `enroll_fs`, `enroll_luks_uuid_ok`, `enroll_luks_dump_slot1_empty`, `enroll_test_passphrase_ok`, `enroll_test_passphrase_fail`, `enroll_test_keyfile_fail`, `enroll_discovery_two_disks`, `enroll_with_mountpoint_ok`, `enroll_make_existing_keyfile`, plus the bare reuses `mock_ok` and `isolated_paths`, plus the **aliased** reuse `use crate::test_fixtures::err_raw as enroll_err_raw;`. The alias is mandatory because `err_raw` collides with the still-extant local `err_raw` (`enroll_key_file.rs:747`); `mock_ok` and `isolated_paths` are imported bare because their local counterparts are named `ok_raw` and `test_paths` respectively, so no collision exists. The migrated tests rewrite their bodies from `by_id(...)` / `passphrase(...)` / `make_membership(...)` / `MockFs::new(&[...])` / `ok_raw(...)` / `err_raw(...)` / `test_paths()` to `enroll_by_id(...)` / `enroll_passphrase(...)` / `enroll_make_membership(...)` / `enroll_fs(&[...])` / `mock_ok(...)` / `enroll_err_raw(...)` / `isolated_paths()` (and so on for the prefixed leaf factories). The unmigrated tests keep calling the still-present locals `by_id` / `passphrase` / etc. without conflict because nothing in the test mod imports those bare names. **Preserve byte-for-byte:** every `assert_eq!(runner.requests(), vec![...])` (1923, 2557), every `iter().filter(...).count()` count assertion (1676-1684, 2425-2441, 1807-1815, 1428-1436), every `// Intent / Why it exists / Scenario` preamble, every `MissingMock`-deferred mock omission. | `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests` green; per-test `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::<name>` green; full `just test-rust` green. The two `assert_eq!(runner.requests(), ...)` tests must pass with the same expected vectors; the `MissingMock` tests must continue to surface `OpenFailed{exit:5}` / `MissingMock` for the deliberately-omitted probes. |
| 3 | **Discovery + dry-run probe + existing-keyfile-validation -- bulk import-only.** Migrate 14 tests: discovery family (6) at 926, 970, 1018, 1067, 1114, 1157; dry-run probe family (5) at 1486, 1555, 1610, 1660 (already in sub-2), 1705 -- so 4 new ones (1486, 1555, 1610, 1705); existing-keyfile validation (3) at 1414, 1448, 1461. Each migrated test rewrites its body to call the prefixed forms: `enroll_fs` / `enroll_by_id` / `enroll_passphrase` / `enroll_make_membership` / `enroll_make_existing_keyfile` / `enroll_discovery_two_disks` / `enroll_luks_uuid_ok` / `enroll_luks_uuid_not_luks` / `enroll_test_keyfile_ok` / `enroll_test_keyfile_fail` / `enroll_with_mountpoint_ok`, plus the bare reuses `mock_ok` / `isolated_paths`, plus the aliased reuse `enroll_err_raw` (`use crate::test_fixtures::err_raw as enroll_err_raw;`). **Preserve byte-for-byte:** every `PreviewNote::PerDisk { ... }` shape match, every `[skip] disk diskN: ...` substring assertion, every `[wait] keyfile: checking against diskN...` / `[ok]   keyfile: already enrolled on diskN` substring + ordering (`pos(wait1) < pos(ok1)` etc.) at 1592-1594, every `report.notes`-survives-Err assertion (1084, 1742-1752), every `runner.requests()` `iter().all(!matches)` (1428-1436). | Per-family runs: `cargo test ... enroll_key_file::tests::plan_discover`, `... ::dry_run`, `... ::validate_existing_keyfile`. Then full `enroll_key_file::tests`. Then `just test-rust`. The `[skip] disk disk1: not present\n` substring (1178) must round-trip; the bracketed-vs-plain stderr split (1183-1184) must round-trip. |
| 4 | **Generate-target-validation + plan_enrollment / plan_single_disk / apply / cmd-level / pure-render -- bulk import-only.** Migrate the long tail: generate-target-validation (5) at 1223, 1255, 1290, 1330, 1378; plan_enrollment / plan_single_disk family (~12 remaining after sub-2) at 1825, 1949, 2000, 2067, 2099, 2160, 2184, 2211, 2455, plus the three `plan_single_disk_*` cases (2160, 2184, 2211); apply (4) at 2638, 2731, 2787 (sub-2 already migrated 2706); cmd_generate_wrong_passphrase_no_keyfile_created (2945); pure-render (2) at 3051, 3080. Generate side-effects (3) at 2836, 2851, 2871 -- `generate_key_file_creates_4096_bytes_mode_400`, `generate_rejects_existing_keyfile`, `generate_key_file_create_new_rejects_existing` -- are direct-`fn` tests using `tempfile::tempdir()` and `super::generate_key_file(...)` with no fixture imports; they need only an `enroll_by_id` swap if they reference `by_id` (none do today). Imports cover the remaining prefixed leaf factories: `enroll_add_keyfile_ok`, `enroll_luks_dump_slot1_occupied`, `enroll_test_passphrase_ok`, `enroll_test_passphrase_fail`, `enroll_test_keyfile_ok`, `enroll_mountpoint_ok`, `enroll_mountpoint_fail`, `enroll_with_mountpoint_fail`. **Preserve byte-for-byte:** every `paths.luks_headers_dir().join(...).exists()` real-fs assertion (2687-2698, 2767-2777), every `!kf.exists()` no-side-effect assertion (1367, 2986, 3041), every backup-failure enriched-error substring (`"cryptsetup luksHeaderBackup --header-backup-file"`, `"after the LUKS mutation completed"` at 2818-2825), every `meta.len() == 4096` / `meta.permissions().mode() & 0o777 == 0o400` (2861-2862), every `runner.requests() == vec![CmdRequest::MountpointCheck { ... }]` early-exit assertion (1361-1366, 1396-1402). | Per-family runs, then full `enroll_key_file::tests`, then `just test-rust`. Real-fs side effects must match: every `paths.luks_headers_dir().join("braid-diskN.luksheader").exists()` returns true post-`apply_enrollment` for `NeedsEnroll` items and false for `AlreadyEnrolled`. |
| 5 | **Cleanup**: delete the now-unused locals in `enroll_key_file.rs::tests`: `test_paths` (694), `MockFs` (700-728), `by_id` (730), `passphrase` (734), `ok_raw` (738), `err_raw` (747), `mountpoint_ok` (756), `mountpoint_fail` (766), `with_mountpoint_ok` (776), `with_mountpoint_fail` (781), `make_membership` (786), `luks_uuid_ok` (796), `luks_uuid_not_luks` (808), `luks_dump_slot1_empty` (821), `luks_dump_slot1_occupied` (833), `test_passphrase_ok` (845), `test_passphrase_fail` (858), `test_keyfile_ok` (875), `test_keyfile_fail` (885), `enroll_ok` (895), `make_existing_keyfile` (1193), `discovery_two_disks` (1203). Drop `use crate::probe::Filesystem;` import in `enroll_key_file.rs::tests` (no longer needed once the local `MockFs` impl is removed). Remove `#[allow(dead_code)]` annotations on `test_fixtures::enroll_key_file` items now that every helper has a consumer. The migrated tests **keep** calling the prefixed forms (`enroll_by_id`, `enroll_make_membership`, ...) AND the aliased `enroll_err_raw`; cleanup does NOT rename them back to bare names and does NOT remove the `use crate::test_fixtures::err_raw as enroll_err_raw;` line. Confirm `cargo check --manifest-path cli/Cargo.toml --tests` is clean (no dangling refs, no `unused_imports` warnings). | No dangling references; full `just test-rust` green; `cargo build` clean (no `unused_imports` / `dead_code` warnings). |

### Sample migration (sub-commit 2, enroll_key_file.rs:2515 -- divergent-passphrase test)

Before (~40 lines, today's body):

```rust
#[test]
fn plan_divergent_passphrase_existing_keyfile_errors_on_disk2() {
    let d1 = "/dev/disk/by-id/d1";
    let d2 = "/dev/disk/by-id/d2";
    let kf = "/tmp/braid.key";
    let pass = "testpass";

    let (tp1_req, tp1_stdin, tp1_out) = test_passphrase_ok(d1, pass);
    let (tp2_req, tp2_stdin, tp2_out) = test_passphrase_fail(d2, pass);

    let runner = MockRunner::default()
        .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
        .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

    let candidates = vec![
        ("disk1".to_owned(), by_id(d1)),
        ("disk2".to_owned(), by_id(d2)),
    ];

    let err = plan_enrollment(
        &runner, &candidates, Path::new(kf),
        &passphrase(pass), EnrollmentPlanMode::ExistingKeyfile,
    ).expect_err("expected divergent passphrase on disk2 to abort planning");

    // ... assertions unchanged ...
}
```

After (sub-commit 2; per-test imports added at the top of the test mod, helper calls swap to the prefixed names):

```rust
// New use line at the top of `mod tests` (added by sub-commit 2):
use crate::test_fixtures::{
    enroll_by_id, enroll_passphrase, enroll_test_passphrase_ok, enroll_test_passphrase_fail,
};

#[test]
fn plan_divergent_passphrase_existing_keyfile_errors_on_disk2() {
    let d1 = "/dev/disk/by-id/d1";
    let d2 = "/dev/disk/by-id/d2";
    let kf = "/tmp/braid.key";
    let pass = "testpass";

    let (tp1_req, tp1_stdin, tp1_out) = enroll_test_passphrase_ok(d1, pass);
    let (tp2_req, tp2_stdin, tp2_out) = enroll_test_passphrase_fail(d2, pass);

    let runner = MockRunner::default()
        .with_output_stdin(tp1_req, tp1_stdin, tp1_out)
        .with_output_stdin(tp2_req, tp2_stdin, tp2_out);

    let candidates = vec![
        ("disk1".to_owned(), enroll_by_id(d1)),
        ("disk2".to_owned(), enroll_by_id(d2)),
    ];

    let err = plan_enrollment(
        &runner, &candidates, Path::new(kf),
        &enroll_passphrase(pass), EnrollmentPlanMode::ExistingKeyfile,
    ).expect_err("expected divergent passphrase on disk2 to abort planning");

    // ... assertions unchanged ...
}
```

The migration's per-test diff is exactly the bare-name -> `enroll_`-prefixed-name swap, plus a single `use` line at the test-mod header. Crucially, the local helper functions `by_id` (730), `passphrase` (734), `test_passphrase_ok` (845), `test_passphrase_fail` (858) **stay in place** through sub-commits 2-4 because unmigrated tests still call them by their bare names; the prefixed `use` imports do not collide with the same-named locals because they are differently-named symbols (`enroll_by_id` vs `by_id`). Sub-commit 5 deletes the locals once every test has been migrated to the prefixed forms. The `// Intent / Why / Scenario` preamble (lines 2496-2514), the `assert_eq!(runner.requests(), ...)` block (2557-2567), and the `MissingMock` deferral (no `LuksDump` mock for d2, comment at 2525-2528) round-trip byte-for-byte.

## Critical files to modify

- `/Users/dan/Code/braid/cli/src/test_fixtures/enroll_key_file.rs` -- NEW. Items per §A.
- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- add `mod enroll_key_file;` (private) and `#[allow(unused_imports)] pub(crate) use enroll_key_file::{...}` facade re-exports for the items the test mod consumes. The `unused_imports` allow follows the existing pattern at lines 56-82 -- it is required because consumers land in later sub-commits (2-4) and `cargo check --tests` would otherwise fail on the unconsumed re-exports during the staggered rollout. Update the module-level doc-comment at lines 1-46 to mention the new scope (one bullet, mirroring the `mount` bullet at 30-36).
- `/Users/dan/Code/braid/cli/src/enroll_key_file.rs` -- delete the inline scaffolding listed in sub-commit 5 (lines 694-908, 1189-1211) and replace local references with `use crate::test_fixtures::{...}` facade imports per the table. Remove the `use crate::probe::Filesystem;` import in the `mod tests` header once the local `MockFs` impl is gone.

No production source changes. No `shared.rs` changes (`shared::MockFs::unmounted` and `shared::mock_ok` are already in place from prior migrations).

## Existing functions / utilities reused

- `shared::MockFs::unmounted` (`test_fixtures/shared.rs:60`) -- already implements `Filesystem`; the enroll fixture wraps it via `enroll_fs(paths: &[&str])` for ergonomic per-test calls. Mirrors `mount::mount_fs` (`test_fixtures/mount.rs:51`).
- `shared::mock_ok(cmd, stdout)` (`test_fixtures/shared.rs:23`, re-exported at `test_fixtures.rs:82`) -- byte-identical to enroll's local `ok_raw`. Migrated tests `use crate::test_fixtures::mock_ok;` and call `mock_ok(...)`. No new enroll wrapper.
- `mount::err_raw(cmd, exit_code, stderr)` (`test_fixtures/mount.rs:81`, re-exported at `test_fixtures.rs:66`) -- byte-identical to enroll's local `err_raw`. Migrated tests **import via alias**: `use crate::test_fixtures::err_raw as enroll_err_raw;` and rewrite call sites to `enroll_err_raw(...)`. The alias is mandatory: a bare `use crate::test_fixtures::err_raw;` would conflict with the still-extant local `fn err_raw` in `enroll_key_file.rs::tests` (line 747) during the staged migration -- the same Rust duplicate-definition rule that motivates the `enroll_` prefix on the new module's exports. Aliasing on import keeps the alias-stable form (`enroll_err_raw`) at every migrated call site, matching the rest of the module's `enroll_*` surface. No new enroll wrapper. No new facade re-export. The alias stays after sub-commit 5 deletes the local; do not "simplify" it back to `err_raw` at call sites.
- `doctor::isolated_paths()` (`test_fixtures/doctor.rs:26`, re-exported at `test_fixtures.rs:61`) -- byte-identical to enroll's local `test_paths`: returns `(TempDir, StatePaths)` over `StatePaths::custom(dir.path().to_owned())`. Migrated tests `use crate::test_fixtures::isolated_paths;` and call `isolated_paths()`.
- `cmd::MockRunner::with_output` / `with_output_stdin` / `with_luks_dump_text_luks2_for` / `with_mappers_closed` / `with_mapper_closed` (`cmd.rs:988`, `1004`, `1126`, `1138`, `1111`) -- the canonical chaining surface; `enroll_discovery_two_disks` is a single composition over these.
- `cmd::MockRunner::with_handler` (`cmd.rs:1021`) -- exists, but **deliberately not used** by the enroll fixture (a broad handler would resolve `MissingMock` probes that ten of the migrated tests rely on). Reserved for per-test override at the call site if a future enroll test ever needs cross-cutting field-based dispatch.
- `MockRunner::with_output_stdin`'s `HashMap` insert behavior (`cmd.rs:1004-1014`) -- already pinned by the regression test `mock_runner_with_output_stdin_override_after_base_wins` landed by the mount migration. Enroll does not introduce a base preflight, so it does not exercise the override path; the existing pin still applies and no new cmd.rs test is required.

## Out of scope for this plan

- Touching `cli/src/enroll_key_file.rs` production code (lines 1-684). This is a pure test-side refactor.
- Migrating other command modules (`add.rs`, `replace.rs`, `unlock.rs`, etc.) -- enroll is the next migration target; siblings come in follow-up plans.
- Building an `EnrollPool` / `EnrollTopology` handler installer or `EnrollKeyFileParamsBuilder` (rejected in §A; load-bearing `MissingMock` and per-test request-set diversity make broad handlers and builders the wrong shape).
- Promoting `enroll_add_keyfile_ok` / `enroll_make_existing_keyfile` / `enroll_make_membership` to `shared`. Each has only one in-tree consumer today (the enroll test mod). If `add` or `replace` later grow `--enroll`-keyfile coverage that needs them, promote then -- and at that point reconsider the `enroll_` prefix on the names that escape the scope.
- Promoting `enroll_passphrase(s) -> Passphrase` or `enroll_by_id(p) -> ByIdPath` to `shared`. Other modules inline the construction. Move only when a second consumer appears.
- Stripping the `enroll_` prefix from helpers whose names don't directly collide today (e.g. `enroll_luks_dump_slot1_empty`). The prefix is applied uniformly across the new module's exports for two reasons: (a) it removes the staged-migration duplicate-definition hazard for every helper, even ones whose name happens not to collide at the facade today; (b) it gives the scope a recognisable shared shape so a `grep enroll_` walks the entire fixture surface. Selective de-prefixing would invite the kind of "is this load-bearing or not?" question this plan exists to settle.
- Adding a new `cmd.rs` regression test. Mount's `mock_runner_with_output_stdin_override_after_base_wins` already pins the static-key overwrite contract; enroll does not introduce a base preflight, so no new contract is exercised.
- Renaming the per-test passphrase strings (`"testpass"`, `"wrongpass"`, `"pass-disk2"`, etc.) to share `shared::TEST_PASSPHRASE_BYTES`. Enroll tests vary the passphrase per scenario; unifying would either lose the per-test wording or force every `with_output_stdin` call to thread the same constant. Keep per-test inline literals; the `(req, stdin, out)` triple factories accept the bytes as an argument.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | Swapping the local `MockFs` (read_to_string returns `NotFound`, is_block_device returns false, list_dir returns empty) for `shared::MockFs::unmounted` (read_to_string returns the rootfs-only mountinfo body for `/proc/self/mountinfo`) silently changes a test's behavior if any production path on the enroll call graph reads `/proc/self/mountinfo`, queries `is_block_device`, or `list_dir`. | Sub-commit 1 includes a verification grep: `grep -nE "fs\.(read_to_string\|is_block_device\|list_dir)" cli/src/{enroll_key_file,probe,luks,credential_verify}.rs`. The grep returns no matches today. If a future change adds one, the impact is contained -- the `unmounted` body says "/" rootfs is mounted (not /mnt/storage), which is the correct answer for the "pool not yet mounted" scenario every enroll test models. The same audit applied to mount's migration; the result is identical here. |
| 2 | Promoting `enroll_discovery_two_disks` masks a future regression where a dry-run / non-generate planning code path adds a probe (e.g. a fresh `CryptsetupIsLuks`) that the test author didn't intend to seed. | The promoted helper preserves the exact set of seeded requests from `enroll_key_file.rs:1203-1211` verbatim: `enroll_luks_uuid_ok` x2, `with_luks_dump_text_luks2_for(&[d1, d2])`, `with_mappers_closed(&["braid-disk1", "braid-disk2"])`. It does NOT seed `CryptsetupIsLuks` or `CryptsetupLuksDump` (the JSON variant). If a future change adds either, the `dry_run_*` and `non_generate_*` tests that consume `enroll_discovery_two_disks` will surface `MissingMock` and the test fails loudly. Add a one-paragraph doc comment on `enroll_discovery_two_disks` listing what it does NOT seed. |
| 3 | The eleven load-bearing-invariant tests' `assert_eq!(runner.requests(), vec![...])` and `iter().filter(...).count()` assertions break if the migration accidentally changes which requests get logged. | Sub-commit 2 is import-only -- the leaf factories are byte-identical to today's locals (`luks_uuid_ok`, `test_passphrase_ok`, `test_passphrase_fail`, `test_keyfile_fail`, `enroll_ok`, etc. each emit the exact `(CmdRequest, ...)` shape today's locals do, verified by `git diff` body comparison in the new module). No `with_handler` is introduced. `MockRunner::run` always logs (`cmd.rs:1172-1175`) regardless of how dispatch resolves. Behavior is preserved by construction. |
| 4 | Migration accidentally drops a `// Intent / Why it exists / Scenario` preamble during a test rewrite. | AGENTS.md's "Test Conventions" section makes the preamble part of the test contract. Verification (per sub-commit) includes `git log -p cli/src/enroll_key_file.rs` -- the diff for each migrated test must show body changes only (the `use ...` import line and the local-helper -> facade-name swaps), with preamble lines unchanged. |
| 5 | A reviewer reads the new `enroll_fs(&[...])` thin wrapper and decides to "simplify" by inlining `shared::MockFs::unmounted(...)` everywhere, then later breaks the convention. | The wrapper exists for ergonomics (`&[&str]` vs `Vec<String>`) and as a single point of change if enroll tests ever need a different shared mock variant. Add a one-line `pub(crate) fn` doc explaining both. Mirrors the same wrapper rationale at `mount.rs:42-50`. |
| 5b | A reviewer reads the `enroll_*` prefix on every helper (and the `err_raw as enroll_err_raw` import alias) and decides to "simplify" the names back to bare forms (`by_id`, `passphrase`, `make_membership`, `err_raw`, ...) because mount and doctor mostly ship unprefixed names. | The prefix and the alias are load-bearing for two distinct reasons captured at the top of the new module's doc comment AND in a one-line comment alongside the `use crate::test_fixtures::err_raw as enroll_err_raw;` import line at every call site that needs it: (a) facade collisions with `doctor::mountpoint_*` and `mount::err_raw` / `luks_uuid_ok` / `test_passphrase_fail` / `ok_raw`; (b) the staged migration's same-module `use` + local `fn` duplicate-definition error (which ALSO applies to the `err_raw` reuse, since the local helper is named `err_raw` -- not `mock_ok` / `isolated_paths`, whose locals are differently named and don't need the alias). Any de-prefixing or de-aliasing breaks one or both. The module-level doc comment quotes these constraints so a future reviewer doesn't try a sweep without re-deriving the rationale. |
| 6 | The `(CmdRequest, Vec<u8>, RawCommandOutput)` triple shape returned by `test_passphrase_ok` / `test_passphrase_fail` / `enroll_ok` is unfamiliar relative to the pair-shape used elsewhere (mount returns pairs and pulls stdin from a constant); a reader migrating a similar pattern in another module copies the wrong shape. | The triple shape is the right tool for enroll because each test passes a different passphrase (testpass / wrongpass / pass-disk2 / etc.), and it matches the existing local helper signature so the migration is purely a name swap. Document the shape choice at the top of the new module's `Triple factories` section: "tests vary passphrase per scenario, so the bytes are an argument; `with_output_stdin(req, stdin, out)` consumes the triple." |
| 7 | The 3 cmd-level tests (`cmd_enroll_blocked_in_recovery_mode`, `cmd_generate_wrong_passphrase_no_keyfile_created`, `cmd_generate_dry_run_short_circuits`) construct `EnrollKeyFileParams` inline; the migration leaves that inline. A future contributor reads them, decides "this looks like boilerplate", and writes a builder. | The three tests are heterogeneous (recovery-mode gate / wrong-passphrase early abort / dry-run short-circuit) and configure different fields per scenario (`generate=false` vs `true`, `passphrase_file=Some(...)` vs `None`, `dry_run=false` vs `true`). A builder for three call sites with no shared default would add boilerplate without saving lines. Document the no-builder decision in the new module's intentional-omissions list (§A) so the rationale is visible at the obvious place to look. |
| 8 | The real-fs apply / generate / cmd tests that assert `paths.luks_headers_dir().join(...).exists()` or `!kf.exists()` are rewritten to use a `MockFs` and silently lose their side-effect coverage. | Sub-commit 4 explicitly preserves these tests' real-fs construction: `tempfile::TempDir::new()` + `std::fs::write` + `paths.luks_headers_dir()` round-trips remain inline. Only the runner / membership / passphrase / by-id construction comes through the fixture. The verification step includes `grep -n 'paths.luks_headers_dir' cli/src/enroll_key_file.rs` post-migration -- the count must match today's count (10 occurrences). |

## Verification

End-to-end gate: `just test-rust` is green at every sub-commit boundary. `test-rust` (Justfile:104) runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard` as a fixed command. Filtered runs go through `cargo test` directly.

**Pre-sub-commit-1 verification (one-time):**

```
grep -nE "fs\.(read_to_string|is_block_device|list_dir)" cli/src/{enroll_key_file,probe,luks,credential_verify}.rs
```

Confirms the only `Filesystem` trait method called in production by the enroll-test call graph is `fs.exists()`. The grep returns no matches today. If this surfaces a `read_to_string` or `is_block_device` call we did not anticipate, abort the swap to `shared::MockFs::unmounted` and ship a scope-local `MockFs` instead.

**Per sub-commit:**

- **Sub-commit 1** (scaffolding): `cargo check --manifest-path cli/Cargo.toml --tests` clean (no `unused_imports` / `dead_code` errors -- the `#[allow(...)]` annotations cover the staggered consumer rollout). Then `just test-rust` green.
- **Sub-commit 2** (load-bearing-invariant family, 11 tests, import-only): per-test runs --
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_all_already_enrolled`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_divergent_passphrase_existing_keyfile_errors_on_disk2`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_generate_new_does_not_repeat_first_candidate_passphrase_verify`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_generate_new_skips_keyfile_probe`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_divergent_passphrase_generate_new_errors_on_disk2`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::apply_skips_already_enrolled`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::cmd_enroll_blocked_in_recovery_mode`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::cmd_generate_dry_run_short_circuits`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::dry_run_with_generate_skips_probe`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::real_run_does_not_probe_before_passphrase`
  Then `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests`. Then `just test-rust`. The two `assert_eq!(runner.requests(), vec![...])` tests must pass with their original expected vectors. The deliberate `MissingMock` tests must continue to surface `OpenFailed{exit:5}` (`plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds`) or `EnrollKeyFileError::Validation("wrong passphrase ...")` (`plan_divergent_passphrase_*`) -- not `MissingMock`. If any of these flips, the leaf factory in the new module emits a different `(CmdRequest, ...)` shape than the local one and must be corrected before the sub-commit lands.
- **Sub-commit 3** (discovery + dry-run probe + existing-keyfile validation, 14 tests, import-only): per-family --
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_discover` (matches all 6 discovery tests)
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::dry_run` (matches dry_run_skips_already_enrolled_disks, dry_run_all_already_enrolled_emits_zero_steps, dry_run_probe_error_propagates, plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status, plus rendering tests already migrated in sub-4)
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::validate_existing_keyfile`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::non_generate_plan_does_not_require_mountpoint`
  Then full `enroll_key_file::tests`, then `just test-rust`. The bracketed-vs-plain stderr render (1183-1184), the `pos(wait1) < pos(ok1)` ordering (1592-1594), and the `report.notes`-survives-Err contracts (1084, 1742-1752) must round-trip.
- **Sub-commit 4** (long tail bulk migration): per-family --
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::generate_rejects` and `generate_dry_run_rejects` (target-validation family)
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::plan_all_need_enroll`, `plan_mixed_enrolled`, `plan_wrong_passphrase`, `plan_slot1_conflict`, `plan_single_disk`, `plan_enrollment_existing_keyfile`, `plan_generate_new_slot1_conflict_errors`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::apply_enrolls`, `apply_mixed_plan`, `apply_enrollment_returns_enriched_error_when_backup_fails`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::cmd_generate_wrong_passphrase_no_keyfile_created`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::dry_run_render`
  - `cargo test --manifest-path cli/Cargo.toml --lib enroll_key_file::tests::generate_key_file_creates_4096_bytes_mode_400` and the two `generate_*rejects_existing` cases (real-fs only; should be unaffected)
  Then full `enroll_key_file::tests`, then `just test-rust`. Real-fs side effects must match: `paths.luks_headers_dir().join("braid-disk1.luksheader").exists()` is true after `apply_enrolls_needs_enroll_items` and false after `apply_mixed_plan` for disk1 (`AlreadyEnrolled`); `!kf.exists()` after `cmd_generate_wrong_passphrase_no_keyfile_created` and `cmd_generate_dry_run_short_circuits`; `meta.len() == 4096` and `meta.permissions().mode() & 0o777 == 0o400` after `generate_key_file_creates_4096_bytes_mode_400`.
- **Sub-commit 5** (cleanup): `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references and no `unused_imports` / `dead_code` warnings. `cargo build --manifest-path cli/Cargo.toml --tests` clean. `just test-rust` full suite green. The `#[allow(dead_code)]` annotations on `test_fixtures::enroll_key_file` items are removed and `cargo build` still clean.

**Behavior-preservation check (mechanical, all sub-commits):**

- Every `// Intent / Why it exists / Scenario` preamble round-trips byte-for-byte. `git log -p cli/src/enroll_key_file.rs` per sub-commit -- diff for each migrated test shows body changes only.
- Every `assert!(...)` / `assert_eq!(...)` body is unchanged across the migration -- the migration touches setup code (runner, fs, params, by-id, passphrase construction) only.
- Every `runner.requests().iter().any(...)`, `runner.requests().iter().filter(...).count()`, and `assert_eq!(runner.requests(), vec![...])` assertion observes the same request log, since the migration does not introduce `with_handler` and `MockRunner::run` always logs.
- Every `paths.luks_headers_dir()` real-fs assertion preserved (10 occurrences today; count must match post-migration).
- Every `!kf.exists()` no-side-effect assertion preserved (3 occurrences today; count must match).
- Every `with_output_stdin` byte-string ("testpass", "wrongpass", per-test variants) preserved at call sites; the new triple factories accept the bytes as an argument so call sites don't change shape.

No new VM tests, no parser-fixture refresh, no production behavior change. The existing test suite IS the verification.

## Branch and commit shape

Work on a feature branch (e.g. `refactor-enroll-key-file-test-fixtures`). Each numbered sub-commit above is one git commit. PR opens once sub-commit 5 lands. Reviewer can walk the branch commit-by-commit; each commit is independently green.

Conventional Commits-style messages (lowercase first word per AGENTS.md):

- `refactor(test): add enroll-key-file scope test fixture module` (sub-commit 1)
- `refactor(enroll-key-file): migrate load-bearing-invariant tests to shared fixtures` (sub-commit 2)
- `refactor(enroll-key-file): migrate discovery and dry-run tests to shared fixtures` (sub-commit 3)
- `refactor(enroll-key-file): migrate plan/apply/cmd tests to shared fixtures` (sub-commit 4)
- `refactor(enroll-key-file): drop migrated locals from enroll-key-file tests module` (sub-commit 5)
