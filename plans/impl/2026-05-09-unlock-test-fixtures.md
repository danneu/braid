# Plan: Migrate `cli/src/unlock.rs` test scaffolding to a shared `test_fixtures::unlock` module

**Status: Draft**

## Context

`cli/src/unlock.rs` is ~1911 lines, of which lines 236-1911 (the `#[cfg(test)] mod tests` block) hold 11 tests plus ~80 lines of inline scaffolding: `test_paths` (249), `MockFs` struct + ctors (259-296) plus the two mountinfo body constants `MOUNTINFO_BTRFS` (255) and `MOUNTINFO_WITHOUT_TARGET` (257), `ok_raw` (298), `err_raw` (307), `three_disk_config` (316), `three_disk_membership` (320). Nine of the eleven tests then construct an in-line `MockRunner` chain that repeats some combination of: `MountpointCheck` not-mounted, three `CryptsetupLuksUuid` probes, `with_luks_dump_text_luks2_for`, `with_mappers_closed`, two-or-three `with_output_stdin(CryptsetupTestPassphrase, b"testpass", ok_raw)` verify-pass calls, two-or-three `with_output_stdin(CryptsetupLuksOpen, b"testpass", ok_raw)` mapper opens, `BtrfsDeviceScanAll`, `Mount` or `MountWithOptions { options: ["degraded"] }`, and `BtrfsBalanceStatus`. Six tests also write `b"testpass"` to a `tempfile::NamedTempFile` for `passphrase_file`.

The 11 tests cluster into five behavior families:

- **Bricked-disk Mount-vs-MountWithOptions invariant** (2, lines 343-622) -- `unlock_bricked_disk_uses_degraded_mount` (3-disk RAID1, disk3 LUKS header zeroed; `--allow-degraded` => `MountWithOptions { options: ["degraded"] }` must be the seeded mount mock; if production regresses to plain `Mount`, `MockRunner` returns `MissingMock` and the test fails) and `unlock_bricked_disk_refuses_without_flag` (same topology without the flag => `UnlockError::Mount(MountError::DegradedRefused(_))` and the error must contain `"refusing to mount degraded"` and the `"--allow-degraded"` hint; no mount mock seeded).
- **2-disk full-execution preserves-error contracts** (2, 639-959) -- `passphrase_mismatch_names_failing_disk` (disk1 verify+open ok, disk2 verify fails => error names `disk2`, does NOT name `disk1`, does NOT say `"Wrong passphrase?"`) and `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` (mount fails after both opens succeed; cleanup `cryptsetup close` for disk1 returns `busy`; primary error stays `MountError::MountFailed("mount failed (exit 32): wrong fs type")`, captured stderr contains `"cleanup failed: one or more LUKS mappers opened by this command"`, and the cleanup loop still attempts `CryptsetupClose { mapper: "braid-disk2" }` after the disk1 close fails).
- **Post-mount enrichment / paused-balance / all-mappers-open** (3, 968-1130, 1577-1769, 1779-1911) -- `unlock_warns_on_paused_balance` (3-disk healthy unlock + post-mount `BtrfsBalanceStatus` returns the paused-balance body; the test's only assertion is `result.expect("unlock should succeed even with paused balance")` at `unlock.rs:1121` -- it pins the success path, not the warning text. The body-to-warning contract is pinned by a separate focused test `unlock_btrfs_balance_status_paused_classifies_as_paused` introduced in sub-commit 1; see §A's note on `unlock_btrfs_balance_status_paused`), `unlock_tolerates_post_mount_probe_mounted_false` (3-disk healthy unlock + post-mount `probe_pool` sees no `/mnt/storage` in `/proc/self/mountinfo` => unlock returns Ok and pool.json fields stay `None`; uses the rootfs-only mountinfo body), `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (3-disk all-mappers-already-open => mount-only branch; `passphrase_file: Some(&bogus)` for a sentinel `PathBuf::from("/definitely/not/a/real/path/passphrase")` proves credential resolution never runs).
- **Dry-run preview rendering / note ordering** (4, 1139-1568) -- `plan_unlock_dry_run_render_2_closed_disks` (probe notes render before the step block; both `[ok]   disk diskN: found\n` lines appear before `"btrfs device scan"`), `plan_unlock_dry_run_render_2_closed_disks_with_key_file` (preview contains `"cryptsetup open --type luks --key-file /run/keys/braid.key --keyfile-size 4096"`, must NOT contain `"--key-file=-"`), `plan_unlock_note_only_success_when_already_mounted` (`MountpointCheck` returns ok => `plan.open_plan` is `None`, steps empty, rendered output is exactly `"pool already mounted at /mnt/storage\n"`), `plan_unlock_preserves_notes_on_degraded_refused` (3-disk dry-run with disk3 unreadable => report's `notes` survive on the `Err` arm with per-disk `NoteLevel::Ok` for disk1+disk2 and `NoteLevel::Skip` for disk3 in that exact order, error is `DegradedRefused`).

Six of these eleven tests carry load-bearing invariants the migration must preserve byte-for-byte:

- **Mount enum-variant assertion** -- `unlock_bricked_disk_uses_degraded_mount` (435-442) seeds `MountWithOptions { options: vec!["degraded".to_owned()] }` and NOT `Mount`. Any helper that auto-resolves `Mount` would silently invert the assertion; the inline comment at 483-485 spells out the trap. Mirror: `unlock_bricked_disk_refuses_without_flag` deliberately seeds NO mount mock so a regression to "always tries Mount" surfaces as `MissingMock`.
- **Mountinfo body distinction** -- `unlock_tolerates_post_mount_probe_mounted_false` (1577-1769) and `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (1779-1911) construct `MockFs` with `MOUNTINFO_WITHOUT_TARGET` so the post-mount `probe_pool`'s `fs.read_to_string("/proc/self/mountinfo")` reports no `/mnt/storage` entry. Every other test uses `MOUNTINFO_BTRFS`. The choice is observed by the production call graph and is part of the asserted behavior, not a default.
- **Bogus passphrase path proves credential resolution didn't run** -- `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (`unlock.rs:1889`) builds `let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");` and passes `passphrase_file: Some(&bogus)`. Because all mappers are already open, `cmd_unlock` takes the mount-only branch and never touches `resolve_credential`. If a regression made it touch the path, the test fails with a real I/O error -- a separate signal from `MissingMock`.
- **Exact preview strings** -- `plan_unlock_dry_run_render_2_closed_disks` (1210-1227) asserts the substrings `"[ok]   disk disk1: found\n"`, `"[ok]   disk disk2: found\n"`, and the ordering `pos(notes) < pos("btrfs device scan")`. `plan_unlock_dry_run_render_2_closed_disks_with_key_file` (1337-1345) asserts `"cryptsetup open --type luks --key-file /run/keys/braid.key --keyfile-size 4096"` substring AND `!preview.contains("--key-file=-")`. `plan_unlock_note_only_success_when_already_mounted` (1419) asserts `assert_eq!(rendered, "pool already mounted at /mnt/storage\n")` byte-for-byte.
- **Note ordering on the `Err` arm** -- `plan_unlock_preserves_notes_on_degraded_refused` (1515-1549) asserts the report `.notes` are emitted in disk1 (`NoteLevel::Ok`) -> disk2 (`NoteLevel::Ok`) -> disk3 (`NoteLevel::Skip`) order even though the `Err` arm aborts before mount.
- **Cleanup ordering on failure** -- `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` asserts `runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupClose { mapper: ... }))` for `braid-disk2` even after the `braid-disk1` close returned exit 5. The cleanup loop must keep trying mappers in order; promotion of a "broad" close handler that ran them in arbitrary order would silently break this.

Outcome: ship `cli/src/test_fixtures/unlock.rs` as a flat collection of helpers (modeled on `test_fixtures/mount.rs` and `test_fixtures/enroll_key_file.rs` -- no `*Pool` / `*Topology` topology installer, no `UnlockParamsBuilder`). The test surface is dominated by per-test request-set composition layered on top of `mount`'s already-shared 2-disk preflight; the four tests that need 3-disk preflight overlap pairwise but not 3-or-more times, so a `base_three_disk_runner` is below the promote-it threshold and stays as chained leaf helpers. Reuse `shared::MockFs::storage` for the `Filesystem` mock body (its mountinfo bytes are identical to unlock's `MOUNTINFO_BTRFS`) via a thin `unlock_storage_fs(&[&str])` wrapper; reuse `mount::mount_fs(&[&str])` (already exported) for the two tests that need the rootfs-only mountinfo body (its bytes are identical to unlock's `MOUNTINFO_WITHOUT_TARGET`). Reuse the existing facade exports `mount::base_two_disk_runner`, `mount::test_config`, `mount::two_disk_membership`, `mount::three_disk_membership`, `mount::luks_uuid_ok`, `mount::test_passphrase_fail`, `mount::MOUNT_TEST_PASSPHRASE_BYTES`, `mount::ok_raw`, `mount::err_raw` (the last two via aliases), and `doctor::isolated_paths`. Newly-exported helpers carry an `unlock_` prefix. The prefix is load-bearing for the same staged-migration reasons enroll's was: (a) avoiding facade collisions with `mount::ok_raw` / `mount::err_raw` and (b) letting the staged migration import a fixture helper while the same-named local still exists for unmigrated tests, since Rust treats `use foo::bar;` plus a same-named local `fn bar` in the same module as a duplicate-definition error.

Migrate tests in six small sub-commits keeping `just test-rust` green at each boundary. Hard cases (the Mount-vs-MountWithOptions family) first; bulk thereafter; cleanup last.

This is unreleased software (AGENTS.md "No backwards compatibility"), so we delete old scaffolding rather than deprecate it.

## Recommended approach

### A. New module `cli/src/test_fixtures/unlock.rs`

Gated `#[cfg(test)]`; registered in `cli/src/test_fixtures.rs` as a private submodule (`mod unlock;`) with `#[allow(unused_imports)] pub(crate) use unlock::{...}` re-exports through the facade -- matching the existing pattern at `test_fixtures.rs:72-132`. Sibling test code imports via the facade only, e.g. `use crate::test_fixtures::{unlock_storage_fs, unlock_luks_uuid_not_luks, unlock_with_test_passphrase_ok, base_two_disk_runner, test_config, three_disk_membership, isolated_paths, MOUNT_TEST_PASSPHRASE_BYTES, luks_uuid_ok, test_passphrase_fail}; use crate::test_fixtures::{ok_raw as unlock_ok_raw, err_raw as unlock_err_raw};` -- never `crate::test_fixtures::unlock::{...}`, since `mod unlock;` is private to `test_fixtures.rs`. The two `as unlock_*` aliases are mandatory; the bare reuses are imported bare. All items inside the new module are `pub(crate)` and test-only.

**Naming convention: every newly-exported helper carries an `unlock_` prefix.** Two pressures enforce this: (1) the facade already exports `ok_raw` / `err_raw` from `mount` (`test_fixtures.rs:98-104`); unprefixed unlock helpers with the same names would collide at the facade. (2) During the staged migration (sub-commits 2-5), each migrated test's `use crate::test_fixtures::...;` line lands while the same-named local helpers still exist in `unlock.rs::tests` for unmigrated tests -- and Rust treats `use foo::bar;` plus a same-named local `fn bar` in the same module as a duplicate-definition error. Prefixing the imports avoids both collisions. Module-level doc comment explains why this scope ships flat helpers (no topology installer, no params builder) -- the load-bearing `Mount`-vs-`MountWithOptions` enum-variant invariant plus the per-test request-set diversity -- and documents the `unlock_` prefix decision so a future reviewer doesn't try to "simplify" by stripping the prefix.

Items in the module:

```rust
// Filesystem
pub(crate) fn unlock_storage_fs(paths: &[&str]) -> shared::MockFs;
    // Thin wrapper: shared::MockFs::storage(paths.iter().map(...).collect()).
    // Centralises the &str -> String conversion. Safe to use the shared
    // "storage" mountinfo body because its bytes are identical to unlock's
    // local MOUNTINFO_BTRFS (`unlock.rs:255`); shared additionally answers
    // `*/exclusive_operation` reads with `"none\n"`, where the local
    // returned `NotFound`. The pre-sub-1 call-graph audit (Verification)
    // covers `cli/src/{unlock,mount,mount_check,probe,credential,
    // credential_verify,membership}.rs` and confirms the only fs reads
    // reachable from `cmd_unlock` are `/proc/self/mountinfo` (via
    // `mount_check::is_btrfs_mounted` at `mount_check.rs:162-168` and
    // `mount_check::fstype_at_mount_via_fs` at `mount_check.rs:172-178`,
    // both consumed by `probe::probe_pool` and `mount::plan_open_pool`)
    // plus `fs.exists` device-path probes. No `*/exclusive_operation`
    // read appears on the unlock call graph. If a future change adds
    // one, the shared "none\n" answer is the correct "no exclusive op
    // in flight" reply for the unmounted-pool scenario every unlock test
    // models, so the swap is forward-compatible; if a different path
    // (e.g. /sys/..., /etc/...) starts being read, the audit fires and
    // the fixture rebuilds `MockFs` scope-local instead.

// (Reuse mount::mount_fs(&[&str]) for the two tests that need the rootfs-
//  only mountinfo body byte-identical to unlock's MOUNTINFO_WITHOUT_TARGET:
//  unlock_tolerates_post_mount_probe_mounted_false and
//  cmd_unlock_skips_credential_resolution_when_nothing_to_unlock.)

// (CmdRequest, RawCommandOutput) leaf factories
pub(crate) fn unlock_luks_uuid_not_luks(device: &str) -> (CmdRequest, RawCommandOutput);
    // CryptsetupLuksUuid { device } paired with err_raw("cryptsetup luksUUID",
    // 1, "Device is not a valid LUKS device.") -- the bricked-header
    // (PresentNotLuks) probe response. Distinct bytes from
    // `enroll_key_file::enroll_luks_uuid_not_luks`, which encodes a
    // different stderr / exit code; per-scope helpers stay scope-specific.
pub(crate) fn unlock_btrfs_device_scan_ok() -> (CmdRequest, RawCommandOutput);
    // BtrfsDeviceScanAll -> ok_raw("btrfs device scan"). Used by every
    // full-execution test (5+ consumers) -- the one-line factory removes
    // the boilerplate without hiding the request shape.
pub(crate) fn unlock_btrfs_balance_status_idle(mp: &MountPoint) -> (CmdRequest, RawCommandOutput);
    // BtrfsBalanceStatus { mount_point } paired with stdout
    // "No balance found on '/mnt/storage'\n" -- the canonical idle-balance
    // post-mount probe response.
pub(crate) fn unlock_btrfs_balance_status_paused(mp: &MountPoint) -> (CmdRequest, RawCommandOutput);
    // BtrfsBalanceStatus { mount_point } paired with the paused-balance
    // stdout body copied byte-for-byte from `unlock.rs:1082-1090` (today's
    // inline literal in `unlock_warns_on_paused_balance`). The body is
    // what `status::get_balance_report` parses to classify "paused", which
    // in turn drives `status::emit_paused_balance_warning` to emit the
    // warning text and return `true`. **The existing test
    // `unlock_warns_on_paused_balance` asserts only that `cmd_unlock`
    // returns `Ok(())` (`unlock.rs:1120-1121`) -- it does NOT assert the
    // warning text or the parser classification.** Sub-commit 1 therefore
    // lands a separate focused self-test
    // `unlock_btrfs_balance_status_paused_classifies_as_paused` that
    // pipes this helper through a `Vec<u8>` writer and asserts both
    // `warned == true` and the exact warning text from
    // `status.rs:697-700`. The self-test pins the body-to-warning
    // contract that `unlock_warns_on_paused_balance` does not.

// Runner-chaining wrappers (small set, each chains a single stdin-bearing
// or stateful request). Used so per-disk verify+open chains read
// linearly at the call site rather than as 6-line `with_output_stdin`
// blocks. The MOUNT_TEST_PASSPHRASE_BYTES constant is hardcoded inside
// these wrappers because every unlock test today uses b"testpass" for
// every verify+open -- there is no per-test variance to argue for a
// triple-shape factory like enroll has.
pub(crate) fn unlock_with_test_passphrase_ok(runner: MockRunner, device: &str) -> MockRunner;
    // Chains CryptsetupTestPassphrase { device } with
    // MOUNT_TEST_PASSPHRASE_BYTES stdin -> ok_raw("cryptsetup open
    // --test-passphrase"). Layers on top of `mount::base_two_disk_runner`
    // when the test wants to override a single disk, and is the building
    // block for tests that do not use `base_two_disk_runner` at all
    // (the 3-disk family + the bricked-disk family).
pub(crate) fn unlock_with_open_mapper_ok(runner: MockRunner, device: &str, mapper: &str) -> MockRunner;
    // Chains CryptsetupLuksOpen { device, mapper } with
    // MOUNT_TEST_PASSPHRASE_BYTES stdin -> ok_raw("cryptsetup open").
    // Used 6+ times across the bricked-disk and full-execution families.
pub(crate) fn unlock_with_mount_ok(runner: MockRunner, device: &str, mp: &MountPoint) -> MockRunner;
    // Chains CmdRequest::Mount { device, mount_point } -> ok_raw("mount -o
    // noatime,skip_balance"). Distinct helper from
    // unlock_with_mount_degraded_ok; the choice between them IS the
    // load-bearing assertion.
pub(crate) fn unlock_with_mount_degraded_ok(runner: MockRunner, device: &str, mp: &MountPoint) -> MockRunner;
    // Chains CmdRequest::MountWithOptions { device, mount_point,
    // options: vec!["degraded".to_owned()] } -> ok_raw("mount -o
    // noatime,skip_balance,degraded"). The
    // unlock_bricked_disk_uses_degraded_mount test relies on this exact
    // variant: if the production code regresses to plain Mount, the
    // MockRunner returns MissingMock and the test fails. The split-into-
    // two-helpers shape preserves that contract -- a single
    // `unlock_with_mount(runner, device, mp, options: &[&str])` would
    // tempt a future reviewer to seed both variants from one call, which
    // would silently mask the regression.
pub(crate) fn unlock_with_three_mappers_open(runner: MockRunner) -> MockRunner;
    // Chains MockRunner::with_mapper_open three times for braid-disk{1,2,3}
    // with backing /dev/vd{a,b,c} and the canonical
    // aaaaaaaa-/bbbbbbbb-/cccccccc-prefixed UUIDs that
    // unlock_tolerates_post_mount_probe_mounted_false and
    // cmd_unlock_skips_credential_resolution_when_nothing_to_unlock both
    // seed today. Two consumers, but the inline form is 6 lines x 2 tests
    // = 12 lines saved -- worth the helper because every UUID and backing
    // device must match exactly between the two tests for the
    // already-open classification to round-trip.

// Passphrase-file helper
pub(crate) fn unlock_passphrase_file() -> tempfile::NamedTempFile;
    // Creates a NamedTempFile with MOUNT_TEST_PASSPHRASE_BYTES content.
    // Caller passes .path() to UnlockParams. Replaces the 6+ occurrences
    // of the inline `let tmp = NamedTempFile::new().unwrap(); use
    // std::io::Write; tmp.as_file().write_all(b"testpass").unwrap();`
    // boilerplate. Tests that intentionally pass a bogus path
    // (`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`,
    // `passphrase_file: Some(&bogus)` for a sentinel
    // `PathBuf::from("/definitely/not/a/real/path/passphrase")` --
    // `unlock.rs:1889`) keep that inline `PathBuf::from(...)` setup; the
    // bogus-path is the load-bearing assertion, not a candidate for a
    // fixture default.
```

**Reused via existing facade exports (no new declarations):**

- `mount::mount_fs(paths: &[&str]) -> shared::MockFs` -- already exported at `test_fixtures.rs:98-104`. Two consumers: `unlock_tolerates_post_mount_probe_mounted_false` and `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`. Its `shared::MockFs::unmounted` body is byte-identical to unlock's `MOUNTINFO_WITHOUT_TARGET`. **No alias needed**: unlock has no local `mount_fs`.
- `mount::base_two_disk_runner() -> MockRunner` -- already exported. Five consumers in unlock today: `passphrase_mismatch_names_failing_disk` (overrides disk2 verify with `mount::test_passphrase_fail` + a `with_output_stdin` for `CryptsetupLuksOpen` that returns exit 2; since `MockRunner::with_output_stdin` overwrites both `outputs` and `stdin_expectations` for the same `CmdRequest`, the override wins -- pinned by the regression test `mock_runner_with_output_stdin_override_after_base_wins` cited in `mount.rs:253-255`), `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` (chains opens + mount-fail + close-fail on top), `plan_unlock_dry_run_render_2_closed_disks` (planner only -- the verify-pass + mappers-closed seeds are unused but harmless), `plan_unlock_dry_run_render_2_closed_disks_with_key_file` (same), and `plan_unlock_note_only_success_when_already_mounted` (overrides `MountpointCheck` to "mounted"; same overwrite semantics apply to the `with_output` path -- pinned by `mock_runner_with_output_override_after_base_wins` if present, else by the same `with_output_stdin` regression's analogue at the static-key level).
- `mount::test_config() -> Config` -- already exported. Byte-identical to unlock's `three_disk_config`: both call `Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()`. **No alias needed**: unlock has no local `test_config`. Replaces eleven inline `Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()` constructions or `three_disk_config()` calls.
- `mount::three_disk_membership() -> PoolMembership` -- already exported. Byte-identical to unlock's local `three_disk_membership`: both build a `BTreeMap` of `(disk1, /dev/disk/by-id/virtio-disk1)` / `(disk2, /dev/disk/by-id/virtio-disk2)` / `(disk3, /dev/disk/by-id/virtio-disk3)`. **No alias needed**: the local has the same name and gets deleted in sub-commit 6, but during sub-commits 2-5 the migrated tests already use the imported version.

  *Migration note*: because the local `fn three_disk_membership` survives in `unlock.rs::tests` for unmigrated tests during sub-commits 2-5, importing the facade's same-named export bare would collide. Solution: instead of aliasing, the migrated tests' `use` line refers to the facade item under the `unlock_three_disk_membership`-style alias -- i.e. `use crate::test_fixtures::three_disk_membership as unlock_three_disk_membership;` -- and the call sites read `unlock_three_disk_membership()`. Alternatively, the migrated tests inline the call as `crate::test_fixtures::three_disk_membership()` to avoid touching `use` lines. Sub-commit 6 deletes the local, after which the alias can stay (uniform `unlock_*` surface) or be dropped (cosmetic). The plan defaults to the alias for symmetry with `unlock_ok_raw` / `unlock_err_raw`.
- `mount::two_disk_membership() -> PoolMembership` -- already exported. Used by three of unlock's 2-disk tests (`passphrase_mismatch_names_failing_disk`, `cmd_unlock_preserves_mount_error_when_cleanup_close_fails`, plus the dry-run rendering tests); the locals construct it inline with the same disk1/disk2 by-id paths. No name collision in unlock today (unlock has no local `two_disk_membership`); imported bare.
- `mount::luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput)` -- already exported. Replaces the inline `with_output(CryptsetupLuksUuid { device }, RawCommandOutput { stdout: "<uuid>\n", ... })` blocks throughout the bricked-disk + 3-disk family.
- `mount::test_passphrase_fail(device: &str) -> (CmdRequest, RawCommandOutput)` -- already exported. Byte-identical to the inline `err_raw("cryptsetup open --test-passphrase", 2, "No key available with this passphrase.")` used by `passphrase_mismatch_names_failing_disk` for disk2. Single consumer in unlock today.
- `mount::MOUNT_TEST_PASSPHRASE_BYTES` -- already exported. Replaces every `b"testpass".to_vec()` / `b"testpass"` literal in unlock tests (8+ sites). Constant is `b"testpass"`; intentionally NOT `shared::TEST_PASSPHRASE_BYTES` (which is `b"test-passphrase"`). The mount fixture documents this divergence at `mount.rs:174-179`; unlock follows mount's convention because every unlock `with_output_stdin` site writes `b"testpass"` today.
- `mount::ok_raw(cmd: &str) -> RawCommandOutput` -- already exported at `test_fixtures.rs:98-104`. Byte-identical to unlock's local `ok_raw`. Migrated tests **import via alias**: `use crate::test_fixtures::ok_raw as unlock_ok_raw;` and rewrite call sites to `unlock_ok_raw(...)`. The alias is required, not cosmetic: the local `fn ok_raw` in `unlock.rs::tests` (line 298) survives sub-commits 2-5 for unmigrated tests, and `use crate::test_fixtures::ok_raw;` plus the local `fn ok_raw` in the same module is a duplicate-definition error. The alias gives the same call-site shape as `unlock_err_raw` and matches the rest of the migration's `unlock_*`-prefixed factories. Sub-commit 6 deletes the local; the alias **stays** because flipping migrated call sites back from `unlock_ok_raw(...)` to `ok_raw(...)` is unnecessary churn and breaks the module-wide `grep unlock_` audit.
- `mount::err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput` -- already exported. Byte-identical to unlock's local `err_raw`. Migrated tests import via alias: `use crate::test_fixtures::err_raw as unlock_err_raw;` and rewrite call sites to `unlock_err_raw(...)`. Same rationale as `ok_raw`.
- `doctor::isolated_paths() -> (TempDir, StatePaths)` -- already exported at `test_fixtures.rs:84-90`. Byte-identical to unlock's local `test_paths`: returns `(TempDir, StatePaths)` over `StatePaths::custom(dir.path().to_owned())`. Migrated tests `use crate::test_fixtures::isolated_paths;` and call `isolated_paths()` where the local previously called `test_paths()`. **No alias needed**: the local helper is named `test_paths`, not `isolated_paths`.

**What does NOT go in this module** (intentional omissions):

- **No `UnlockTopology` / `UnlockPool` handler installer.** The Mount-vs-MountWithOptions enum-variant assertion (sub-commit 2 family) and the deliberate-no-mount-mock pattern in `unlock_bricked_disk_refuses_without_flag` both rely on `MockRunner` returning `MissingMock` for the wrong / missing variant. A broad `with_handler` would either resolve `Mount` when only `MountWithOptions` was seeded (silently masking a regression) or vice versa. Mirrors `mount.rs`'s and `enroll_key_file.rs`'s rationale.
- **No `UnlockParamsBuilder`.** `UnlockParams<'a>` has 8 fields (`config`, `membership`, `paths`, `passphrase_stdin`, `passphrase_file`, `key_file`, `allow_degraded`, `dry_run`); only 11 tests construct it, and only 4 fields actually vary across them (`passphrase_file: Option<&Path>`, `key_file: Option<...>`, `allow_degraded: bool`, `dry_run: bool`). A builder for 11 call sites with 4 varying fields would add boilerplate without saving lines, and `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` deliberately uses a bogus `passphrase_file` path that a builder would invite "tidying" away.
- **No `base_three_disk_runner` composite.** The 3-disk healthy preflight (mountpoint not-mounted + 3 luksUUID + 3 luks_dump + 3 mappers-closed + 3 verify-pass + 3 mapper-open) is a tempting analogue of `mount::base_two_disk_runner` but only 2 unlock tests share its full shape (`unlock_warns_on_paused_balance` + `unlock_tolerates_post_mount_probe_mounted_false`). The third 3-disk test (`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`) takes the all-mappers-already-open branch and does NOT seed verify/open. Two consumers is below the promote-it threshold, and chaining the three `unlock_with_test_passphrase_ok` / `unlock_with_open_mapper_ok` calls plus `with_luks_dump_text_luks2_for(&[d1, d2, d3])` + `with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"])` keeps the per-test runner readable. If a fourth consumer ever lands, promote then.
- **No `bricked_disk_3_runner` composite.** Same argument: only 2 consumers. The pattern is "2 luksUUID ok + 1 luksUUID not_luks + 2 verify-pass + 2 luks-open + with_luks_dump_text_luks2_for(&[d1, d2]) + with_mappers_closed(&["braid-disk1", "braid-disk2"])"; both bricked-disk tests can chain that explicitly using the leaf helpers above plus `mount::luks_uuid_ok` and `unlock_luks_uuid_not_luks`. If a third bricked-disk test ever lands, promote then.
- **No new `Filesystem` trait impl.** `shared::MockFs::storage` and `shared::MockFs::unmounted` already implement `Filesystem` and match unlock tests' needs once the verification grep confirms unlock's call graph never reads `*/exclusive_operation`. The two existing variants cover both unlock mountinfo bodies byte-for-byte.
- **No promotion of `unlock_*` helpers to `shared`** in this plan. Each helper has a single-scope consumer set today (the unlock test mod). Promote later if a second scope adopts one. Specifically: `unlock_with_test_passphrase_ok` could be argued for promotion since mount's `direct_two_disk_open_runner` does similar work, but mount's helper is a full pre-built runner whereas unlock's is a single chain; different ergonomics. Keep separate.
- **No real-fs assertions migrated into the fixture.** Every test that writes a passphrase to disk (`unlock_passphrase_file()`) keeps the call inline -- the fixture supplies the helper, but the `tmp.path()` -> `UnlockParams::passphrase_file` wiring happens at the call site. Mirrors the apply / generate / cmd-level real-fs handling in the enroll migration.
- **No new `cmd.rs` regression test for runner overwrite semantics.** `mock_runner_with_output_stdin_override_after_base_wins` already pins the override-wins behavior that `passphrase_mismatch_names_failing_disk` and `plan_unlock_note_only_success_when_already_mounted` rely on. Unlock does not introduce a new dispatch path.

### B. Migration ordering principle

Move scaffolding once, then replace local references one family at a time. Hard cases first, bulk second:

- (a) **Bricked-disk Mount-vs-MountWithOptions invariant** is the highest-risk family because the Mount-vs-MountWithOptions enum-variant assertion is the entire point of two of the eleven tests. Migration is import-only -- the leaf factories (`mount::luks_uuid_ok`, `unlock_luks_uuid_not_luks`, `unlock_with_test_passphrase_ok`, `unlock_with_open_mapper_ok`, `unlock_with_mount_degraded_ok`, `unlock_btrfs_device_scan_ok`, `unlock_btrfs_balance_status_idle`, `unlock_passphrase_file`) are byte-identical to today's inline forms; no new dispatch path is introduced. Land first to validate the swap doesn't shift request shapes.
- (b) **2-disk full-execution preserves-error contracts** consume `mount::base_two_disk_runner` plus the `unlock_with_open_mapper_ok` / `unlock_with_mount_ok` chain helpers. `passphrase_mismatch_names_failing_disk` overrides disk2's verify via `mount::test_passphrase_fail` (override-wins semantics). `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` chains the post-open mount-fail + cleanup-close-fail mocks inline.
- (c) **Post-mount enrichment / paused-balance / all-mappers-open** consume `unlock_storage_fs` (or `mount::mount_fs` for the without-target two), the chain helpers, `unlock_btrfs_balance_status_paused`, `unlock_with_three_mappers_open`. The bogus-passphrase-file invariant in `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` stays inline.
- (d) **Dry-run preview rendering / note ordering** -- bulk import-only swap. Tests construct `mount::base_two_disk_runner` (or override `MountpointCheck` to "mounted" for `plan_unlock_note_only_success_when_already_mounted`); preview-string assertions stay inline.
- (e) **Cleanup**: delete the now-unused locals in one commit.

### C. Migration table

| Sub-commit | Action | Validates |
|---|---|---|
| 1 | Land `cli/src/test_fixtures/unlock.rs` with the items in §A (every newly-exported name carries the `unlock_` prefix). Register `mod unlock;` (private) + `#[allow(unused_imports)] pub(crate) use unlock::{...}` facade re-exports in `test_fixtures.rs` (matching the existing groups at `test_fixtures.rs:72-132` -- the `unused_imports` allow is required because consumer migration spans sub-commits 2-5; without it sub-commit 1 fails `cargo check --tests` on the unconsumed re-exports). Mark every item in the new module `#[allow(dead_code)]` since no consumers yet **except** the new focused self-test described below, which provides the first consumer for `unlock_btrfs_balance_status_paused`. Do **not** add a new `ok_raw` / `err_raw` re-export -- mount's existing exports (`test_fixtures.rs:98-104`) are byte-identical and already in the facade; consumers in sub-commits 2-5 import them via the aliases `use crate::test_fixtures::{ok_raw as unlock_ok_raw, err_raw as unlock_err_raw};`. For `mount::three_disk_membership` (already at `test_fixtures.rs:98-104`), consumers either alias as `unlock_three_disk_membership` for symmetry with the rest of the unlock-prefixed surface, or call the facade item path-qualified (`crate::test_fixtures::three_disk_membership()`); pick one approach in §A and apply uniformly. Update `test_fixtures.rs` module-level doc comment to mention the new scope (one bullet, mirroring the `mount` and `enroll_key_file` bullets at lines 31-52). **Pre-sub-1 call-graph audit (replaces the earlier grep gate)**: enumerate every `fs.read_to_string` / `fs.is_block_device` / `fs.list_dir` site reachable from `cmd_unlock` and confirm the only reads are `/proc/self/mountinfo` (answered identically by `shared::MockFs::storage` and the local `MockFs`) plus path-existence checks. The audit covers `cli/src/{unlock,mount,mount_check,probe,credential,credential_verify,membership}.rs`; `cli/src/mount_check.rs` is included because `probe::probe_pool` reaches `mount_check::fstype_at_mount_via_fs` (`mount_check.rs:172-178`) and `mount_check::is_btrfs_mounted` (`mount_check.rs:162-168`), both of which read `MOUNTINFO_PATH` through the trait. The accepted reads are: `/proc/self/mountinfo` (mount_check), `fs.exists(...)` device-path probes (probe.rs / mount.rs). Any other path -- in particular `*/exclusive_operation` -- aborts the swap to `shared::MockFs::storage` and the migration ships a scope-local `MockFs` instead. Run as: `grep -nE "fs\\.(read_to_string\\|is_block_device\\|list_dir)" cli/src/{unlock,mount,mount_check,probe,credential,credential_verify,membership}.rs` and triage each match. **Pin the paused-balance literal**: at sub-commit 1, copy the exact stdout body that `unlock_warns_on_paused_balance` (`unlock.rs:1082-1090`) seeds into `unlock_btrfs_balance_status_paused`. **Land a focused self-test in `cli/src/unlock.rs::tests`**: `unlock_btrfs_balance_status_paused_classifies_as_paused`. The test starts with the canonical `//` line-comment preamble per `docs/testing.md:11-22`:

```rust
// Intent: the unlock_btrfs_balance_status_paused fixture body is the
//   bytes that classify as Paused through the same parser+emitter path
//   cmd_unlock takes post-mount, and the warning text matches the
//   canonical text that production emits.
// Why it exists: unlock_warns_on_paused_balance only asserts Ok(()), so
//   a drift in the fixture's stdout body, in get_balance_report's parser,
//   or in emit_paused_balance_warning's literal text would silently
//   downgrade the warning while the success-path test keeps passing.
//   This pins the body-to-warning contract end-to-end.
// Scenario: feed the fixture's (req, out) pair through a MockRunner into
//   status::emit_paused_balance_warning against a Vec<u8> writer; expect
//   warned == true and the writer body equal to the canonical warning
//   string from status.rs:697-700.
```

Body builds a `MockRunner` with `unlock_btrfs_balance_status_paused(&mp)` as its only seed, calls `crate::status::emit_paused_balance_warning(&runner, &mp, &mut sink)` against a `let mut sink: Vec<u8> = Vec::new();` writer, and asserts (a) the function returns `true`, (b) `String::from_utf8(sink).expect("warning is utf-8")` is `assert_eq!`-equal to the canonical warning string -- modeled on the existing `status.rs::tests::emit_paused_balance_warning_writes_to_buffer` (`status.rs:2422-2428`):

```rust
let expected = concat!(
    "\n",
    "  paused balance detected -- will not auto-resume\n",
    "    resume:  btrfs balance resume /mnt/storage\n",
    "    cancel:  btrfs balance cancel /mnt/storage\n",
);
assert_eq!(output, expected);
```

The full-string `assert_eq!` (not `contains` substring checks) catches extra leading whitespace, missing blank lines, drifted hint phrasing, or any other byte-level deviation from `status.rs:697-700`. This self-test pins the body-to-warning contract that `unlock_warns_on_paused_balance` (which only asserts `Ok(())` at `unlock.rs:1120-1121`) does not, and is unaffected by `unlock_warns_on_paused_balance`'s migration in sub-commit 4 -- it survives unchanged into the cleanup commit. | Module compiles; `cargo check --manifest-path cli/Cargo.toml --tests` clean (no `unused_imports` / `dead_code` errors); `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_btrfs_balance_status_paused_classifies_as_paused` green; `just test-rust` green. The call-graph audit produces only the accepted reads (mountinfo + fs.exists); if a production-code `read_to_string` for any other path surfaces, abort the swap to `shared::MockFs::storage` and ship a scope-local `MockFs` instead. |
| 2 | **Bricked-disk Mount-vs-MountWithOptions family -- import-only migration.** Migrate `unlock_bricked_disk_uses_degraded_mount` (343-486) and `unlock_bricked_disk_refuses_without_flag` (494-622). Per-test imports pull `unlock_storage_fs`, `unlock_luks_uuid_not_luks`, `unlock_with_test_passphrase_ok`, `unlock_with_open_mapper_ok`, `unlock_with_mount_degraded_ok`, `unlock_btrfs_device_scan_ok`, `unlock_btrfs_balance_status_idle`, `unlock_passphrase_file`, plus the bare reuses `isolated_paths`, `test_config`, `luks_uuid_ok`, `MOUNT_TEST_PASSPHRASE_BYTES`, plus the aliased reuses `use crate::test_fixtures::{ok_raw as unlock_ok_raw, err_raw as unlock_err_raw};` (and the chosen alias for `three_disk_membership`). Both tests rewrite their bodies to chain the helpers on top of `MockRunner::default()`: `MountpointCheck` not-mounted (still inline as a one-liner against `unlock_err_raw`), `mount::luks_uuid_ok` for disk1+disk2, `unlock_luks_uuid_not_luks` for disk3, `with_luks_dump_text_luks2_for(&[disk1, disk2])`, `with_mappers_closed(&["braid-disk1", "braid-disk2"])`, two `unlock_with_test_passphrase_ok` calls, two `unlock_with_open_mapper_ok` calls, `unlock_btrfs_device_scan_ok` (only `unlock_bricked_disk_uses_degraded_mount`), `unlock_with_mount_degraded_ok` (only `unlock_bricked_disk_uses_degraded_mount`), `unlock_btrfs_balance_status_idle` (only `unlock_bricked_disk_uses_degraded_mount`). `unlock_bricked_disk_refuses_without_flag` deliberately seeds NO mount mock (the entire point of the test); preserve byte-for-byte. **Preserve byte-for-byte:** the `MountWithOptions { options: vec!["degraded".to_owned()] }` enum-variant choice (435-442), the `// If the code incorrectly uses Mount instead of MountWithOptions, MockRunner returns MissingMock` comment (483-485), the `UnlockError::Mount(MountError::DegradedRefused(_))` arm match (610), and the substring assertions `"refusing to mount degraded"` + `"--allow-degraded"` (614-621). | `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_bricked_disk_uses_degraded_mount`; `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_bricked_disk_refuses_without_flag`. Then `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests`. Then `just test-rust`. The Mount-vs-MountWithOptions distinction must round-trip: deleting the seeded `MountWithOptions` mock (or changing it to `Mount`) must surface `MissingMock` in the first test; absence of any mount mock must surface `DegradedRefused` in the second. |
| 3 | **2-disk full-execution preserves-error contracts -- import migration on top of `base_two_disk_runner`.** Migrate `passphrase_mismatch_names_failing_disk` (639-781) and `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` (790-959). Per-test imports add `base_two_disk_runner`, `test_passphrase_fail`, `two_disk_membership`, `unlock_with_open_mapper_ok`, `unlock_with_mount_ok`, `unlock_btrfs_device_scan_ok`, `unlock_passphrase_file`, plus the bare reuses already imported by sub-2's tests. `passphrase_mismatch_names_failing_disk` rewrites to: build the runner from `base_two_disk_runner()`, then `.with_output_stdin(test_passphrase_fail(disk2).0, MOUNT_TEST_PASSPHRASE_BYTES.to_vec(), test_passphrase_fail(disk2).1)` to override disk2's verify (override-wins semantics; the existing `mock_runner_with_output_stdin_override_after_base_wins` regression test pins this); then `.with_output(CryptsetupIsLuks { device: disk2 }, unlock_ok_raw("cryptsetup isLuks"))`; then chain `unlock_with_open_mapper_ok(disk1, "braid-disk1")`; then layer disk2's open-fail inline (a single `with_output_stdin` for `CryptsetupLuksOpen { device: disk2, mapper: "braid-disk2" }` returning exit 2). `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` rewrites to: build from `base_two_disk_runner()`, chain `unlock_with_open_mapper_ok` for both disks, chain `unlock_btrfs_device_scan_ok`, layer the mount-fail `with_output(Mount { ... }, unlock_err_raw("mount", 32, "wrong fs type"))` inline, layer the cleanup `BtrfsDeviceScanForget` mock, layer `CryptsetupClose` busy/ok mocks inline. **Preserve byte-for-byte:** the error message assertions (`!msg.contains("disk1")`, `msg.contains("disk2")`, `!msg.contains("Wrong passphrase?")`) at 769-780; the `MountError::MountFailed("mount failed (exit 32): wrong fs type")` substring at 933-937; the captured-stderr assertion `"cleanup failed: one or more LUKS mappers opened by this command"` at 941-944; and the `runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupClose { mapper: ... == "braid-disk2" }))` cleanup-still-attempts assertion at 945-958. The captured-stderr block (`status_tag::testing::capture_with_color(false, ...)`) stays inline. | Per-test runs: `cargo test ... unlock::tests::passphrase_mismatch_names_failing_disk`; `cargo test ... unlock::tests::cmd_unlock_preserves_mount_error_when_cleanup_close_fails`. Then `unlock::tests`. Then `just test-rust`. The disk2-named-error contract must round-trip; the cleanup-keeps-trying contract must round-trip. The override-wins behavior on `base_two_disk_runner` (disk2 verify-fail layered on disk2 verify-ok) must produce the rejection, not the success -- regression-pinned by `mock_runner_with_output_stdin_override_after_base_wins`. |
| 4 | **Post-mount enrichment / paused-balance / all-mappers-open -- 3-disk family migration.** Migrate `unlock_warns_on_paused_balance` (968-1130), `unlock_tolerates_post_mount_probe_mounted_false` (1577-1769), and `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (1779-1911). Per-test imports add `unlock_storage_fs`, `mount_fs`, `unlock_btrfs_balance_status_paused`, `unlock_with_three_mappers_open`, plus the chain helpers already imported. The first two tests build their runners by chaining `unlock_with_test_passphrase_ok` x3 and `unlock_with_open_mapper_ok` x3 over `MockRunner::default()`, plus `unlock_btrfs_device_scan_ok`, plus `unlock_with_mount_ok`, plus the per-test balance-status helper (`unlock_btrfs_balance_status_idle` for `unlock_tolerates_post_mount_probe_mounted_false`, `unlock_btrfs_balance_status_paused` for `unlock_warns_on_paused_balance`), plus `with_luks_dump_text_luks2_for(&[d1, d2, d3])` and `with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"])`. `unlock_tolerates_post_mount_probe_mounted_false` uses `mount_fs(&[...])` (not `unlock_storage_fs`) so the post-mount probe sees no `/mnt/storage`; this is the load-bearing assertion -- the comment at the top of the test must explicitly call out the choice. `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` uses `mount_fs(&[...])` plus `unlock_with_three_mappers_open`, and KEEPS its inline sentinel `let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase"); ... passphrase_file: Some(&bogus)` (`unlock.rs:1889`) -- the bogus-path is the load-bearing assertion that credential resolution never runs; do not "tidy" it into the fixture. **Preserve byte-for-byte:** the success-only assertion in `unlock_warns_on_paused_balance` -- `result.expect("unlock should succeed even with paused balance")` at `unlock.rs:1120-1121` -- which is the test's complete assertion set; the warning-text and parser-classification contract is pinned by the sub-commit-1 self-test `unlock_btrfs_balance_status_paused_classifies_as_paused`, not by this test. Also preserve the post-mount probe `mounted=false` -> `pool.json` fields stay `None` assertion, and the bogus-path inline construction. | Per-test runs: `cargo test ... unlock::tests::unlock_warns_on_paused_balance`; `cargo test ... unlock::tests::unlock_tolerates_post_mount_probe_mounted_false`; `cargo test ... unlock::tests::cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`. Then `unlock::tests` (which also re-runs `unlock_btrfs_balance_status_paused_classifies_as_paused` from sub-commit 1). Then `just test-rust`. The bogus-path test must NOT touch the sentinel `/definitely/not/a/real/path/passphrase` (ensured by the all-mappers-open mount-only branch); a regression that called `resolve_credential` here would surface a real I/O error, not `MissingMock`, and the migration would catch it. |
| 5 | **Dry-run preview rendering / note ordering -- bulk import-only.** Migrate `plan_unlock_dry_run_render_2_closed_disks` (1139-1250), `plan_unlock_dry_run_render_2_closed_disks_with_key_file` (1257-1356), `plan_unlock_note_only_success_when_already_mounted` (1366-1442), `plan_unlock_preserves_notes_on_degraded_refused` (1448-1568). Imports cover `base_two_disk_runner` (or chain helpers for the 3-disk preserves-notes test), `two_disk_membership` (or `three_disk_membership`/alias), plus the chain helpers already imported. The first two tests use `base_two_disk_runner` (the verify-pass + mappers-closed seeds are unused for dry-run but harmless). `plan_unlock_note_only_success_when_already_mounted` uses `base_two_disk_runner` and overrides `MountpointCheck` to "mounted" via `.with_output(MountpointCheck("/mnt/storage"), unlock_ok_raw("mountpoint"))` (override-wins on `with_output`). `plan_unlock_preserves_notes_on_degraded_refused` uses chained `mount::luks_uuid_ok` x2 + `unlock_luks_uuid_not_luks` x1 (no verify, no open -- dry-run) plus `with_luks_dump_text_luks2_for(&[d1, d2])` + `with_mappers_closed(&["braid-disk1", "braid-disk2"])`. **Preserve byte-for-byte:** every preview-string substring assertion at 1210-1227, 1337-1345, 1419 (exact equality `"pool already mounted at /mnt/storage\n"`); every `NoteLevel` ordering assertion at 1515-1549. | Per-test runs: `cargo test ... unlock::tests::plan_unlock_dry_run_render_2_closed_disks`; `cargo test ... unlock::tests::plan_unlock_dry_run_render_2_closed_disks_with_key_file`; `cargo test ... unlock::tests::plan_unlock_note_only_success_when_already_mounted`; `cargo test ... unlock::tests::plan_unlock_preserves_notes_on_degraded_refused`. Then `unlock::tests`. Then `just test-rust`. The keyfile preview-string substring assertions (`"--key-file /run/keys/braid.key --keyfile-size 4096"` present, `"--key-file=-"` absent) must round-trip; the `assert_eq!(rendered, "pool already mounted at /mnt/storage\n")` must round-trip exactly; the note ordering assertions (Ok/Ok/Skip in that order) must round-trip. |
| 6 | **Cleanup**: delete the now-unused locals in `unlock.rs::tests`: `test_paths` (249-253), `MOUNTINFO_BTRFS` (255-256), `MOUNTINFO_WITHOUT_TARGET` (257), `MockFs` struct + ctors + `Filesystem` impl (259-296), `ok_raw` (298-305), `err_raw` (307-314), `three_disk_config` (316-318), `three_disk_membership` (320-333). Drop `use crate::probe::Filesystem;` from the `mod tests` header (no longer needed once the local `MockFs` impl is removed). Remove `#[allow(dead_code)]` annotations on `test_fixtures::unlock` items now that every helper has a consumer. The migrated tests **keep** calling the prefixed forms (`unlock_storage_fs`, `unlock_with_test_passphrase_ok`, ...) AND the aliased `unlock_ok_raw` / `unlock_err_raw` (and `unlock_three_disk_membership` if that alias was chosen); cleanup does NOT rename them back to bare names and does NOT remove the alias `use` lines. Confirm `cargo check --manifest-path cli/Cargo.toml --tests` is clean (no dangling refs, no `unused_imports` / `dead_code` warnings). | No dangling references; full `just test-rust` green; `cargo build --manifest-path cli/Cargo.toml --tests` clean. |

### Sample migration (sub-commit 2, `unlock.rs:343` -- `unlock_bricked_disk_uses_degraded_mount`)

Before (~145 lines, today's body, abbreviated; note the existing preamble at `unlock.rs:335-341` is `///` doc-comment form):

```rust
/// Bricked LUKS header (PresentNotLuks) on a known pool member must trigger
/// degraded mount when --allow-degraded is passed.
///
/// Scenario: 3-disk RAID1, disk3's LUKS header is zeroed. Probe sees disk3
/// as PresentNotLuks (device exists, but cryptsetup luksUUID fails). The
/// surviving 2 disks unlock normally. Mount must use `-o degraded` because
/// btrfs will see a missing member device.
#[test]
fn unlock_bricked_disk_uses_degraded_mount() {
    let (_state_dir, sp) = test_paths();
    let config = three_disk_config();
    let membership = three_disk_membership();

    let fs = MockFs::new(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]);

    let runner = MockRunner::default()
        .with_output(
            CmdRequest::MountpointCheck { path: MountPoint("/mnt/storage".to_owned()) },
            err_raw("mountpoint", 1, ""),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid { device: "/dev/disk/by-id/virtio-disk1".into() },
            RawCommandOutput { cmd: "cryptsetup luksUUID".into(), stdout: "aaaa...\n".into(), ..ok },
        )
        // ... ~120 more lines of luksUUID / verify / open / scan / mount / balance chains ...
        .with_luks_dump_text_luks2_for(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-disk2"])
        .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        use std::io::Write;
        tmp.as_file().write_all(b"testpass").unwrap();
    }

    let result = cmd_unlock(&runner, &fs, &UnlockParams { /* ... 8 fields ... */ });
    result.expect("unlock with bricked disk should use degraded mount and succeed");
}
```

After (sub-commit 2; per-test imports added at the top of the test mod, helper calls swap to the prefixed names):

```rust
// New use lines at the top of `mod tests` (added by sub-commit 2):
use crate::test_fixtures::{
    isolated_paths, luks_uuid_ok, test_config, MOUNT_TEST_PASSPHRASE_BYTES,
    three_disk_membership as unlock_three_disk_membership,
    unlock_btrfs_balance_status_idle, unlock_btrfs_device_scan_ok,
    unlock_luks_uuid_not_luks, unlock_passphrase_file, unlock_storage_fs,
    unlock_with_mount_degraded_ok, unlock_with_open_mapper_ok,
    unlock_with_test_passphrase_ok,
};
use crate::test_fixtures::{ok_raw as unlock_ok_raw, err_raw as unlock_err_raw};

// Intent: a bricked LUKS header (PresentNotLuks) on a known pool member must
//   trigger a degraded mount when --allow-degraded is passed.
// Why it exists: a regression that picked plain Mount over MountWithOptions
//   would mount without the degraded flag, and btrfs would refuse the missing
//   member -- silently turning a recoverable boot into a hard failure.
// Scenario: 3-disk RAID1, disk3's LUKS header is zeroed. Probe sees disk3 as
//   PresentNotLuks; the surviving two disks unlock normally; Mount must use
//   `-o degraded`.
#[test]
fn unlock_bricked_disk_uses_degraded_mount() {
    let (_state_dir, sp) = isolated_paths();
    let config = test_config();
    let membership = unlock_three_disk_membership();

    let fs = unlock_storage_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]);

    let mp = MountPoint("/mnt/storage".to_owned());
    let (uuid1_req, uuid1_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk1", "aaaaaaaa-1111-2222-3333-444444444444",
    );
    let (uuid2_req, uuid2_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk2", "bbbbbbbb-1111-2222-3333-444444444444",
    );
    let (uuid3_req, uuid3_out) = unlock_luks_uuid_not_luks("/dev/disk/by-id/virtio-disk3");
    let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
    let (bs_req, bs_out) = unlock_btrfs_balance_status_idle(&mp);

    let runner = MockRunner::default()
        .with_output(
            CmdRequest::MountpointCheck { path: mp.clone() },
            unlock_err_raw("mountpoint", 1, ""),
        )
        .with_output(uuid1_req, uuid1_out)
        .with_output(uuid2_req, uuid2_out)
        .with_output(uuid3_req, uuid3_out);
    let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
    let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
    let runner = unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
    let runner = unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
    let runner = runner.with_output(scan_req, scan_out);
    let runner = unlock_with_mount_degraded_ok(runner, "/dev/mapper/braid-disk1", &mp);
    let runner = runner
        .with_output(bs_req, bs_out)
        .with_luks_dump_text_luks2_for(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ])
        .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

    let tmp = unlock_passphrase_file();

    let result = cmd_unlock(&runner, &fs, &UnlockParams {
        config: &config,
        membership: &membership,
        paths: &sp,
        passphrase_stdin: false,
        passphrase_file: Some(tmp.path()),
        key_file: None,
        allow_degraded: true,
        dry_run: false,
    });

    // If the code incorrectly uses Mount instead of MountWithOptions,
    // MockRunner returns MissingMock -- the test fails.
    result.expect("unlock with bricked disk should use degraded mount and succeed");
}
```

The migration's per-test diff is the ~120 lines of inline `MockRunner` chains compressed to ~25 lines of helper calls, plus the `MockFs::new` -> `unlock_storage_fs` / `test_paths` -> `isolated_paths` / `three_disk_config` -> `test_config` / `three_disk_membership` -> `unlock_three_disk_membership` / `MockFs::with_mountinfo(_, MOUNTINFO_WITHOUT_TARGET)` -> `mount_fs` swaps. Crucially, the local helper functions `MockFs` (259), `test_paths` (249), `ok_raw` (298), `err_raw` (307), `three_disk_config` (316), `three_disk_membership` (320) **stay in place** through sub-commits 2-5 because unmigrated tests still call them by their bare names; the prefixed `use` imports (and the `as unlock_*` aliases) do not collide with the same-named locals because they are differently-named symbols (`unlock_three_disk_membership` vs `three_disk_membership` -- the local stays as `three_disk_membership`, the aliased import is `unlock_three_disk_membership`). Sub-commit 6 deletes the locals once every test has been migrated. The preamble at `unlock.rs:335-341` is today's free-form `///` doc-comment (`/// Bricked LUKS header (PresentNotLuks) on a known pool member must trigger ... Scenario: 3-disk RAID1, disk3's LUKS header is zeroed ...`); sub-commit 2 normalises it to the canonical `// Intent: ... // Why it exists: ... // Scenario: ...` line-comment triple per `docs/testing.md:11-22`, preserving the substantive content (the bricked-header behavior, the regression risk, and the 3-disk scenario) verbatim. The `// If the code incorrectly uses Mount instead of MountWithOptions, MockRunner returns MissingMock` body comment at 483-485 is preserved verbatim because it documents an inline trap, not the test preamble.

## Critical files to modify

- `/Users/dan/Code/braid/cli/src/test_fixtures/unlock.rs` -- NEW. Items per §A. ~150-200 lines including doc comments.
- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- add `mod unlock;` (private) and `#[allow(unused_imports)] pub(crate) use unlock::{...}` facade re-exports for the items the test mod consumes. The `unused_imports` allow follows the existing pattern at lines 83-132 -- it is required because consumers land in later sub-commits (2-5) and `cargo check --tests` would otherwise fail on the unconsumed re-exports during the staggered rollout. Update the module-level doc comment at lines 1-70 to mention the new scope (one bullet, mirroring the `mount` and `enroll_key_file` bullets).
- `/Users/dan/Code/braid/cli/src/unlock.rs` -- delete the inline scaffolding listed in sub-commit 6 (lines 249-333) and replace local references with `use crate::test_fixtures::{...}` facade imports per the table. Remove the `use crate::probe::Filesystem;` import in the `mod tests` header once the local `MockFs` impl is gone.

No production source changes. No `shared.rs` changes (`shared::MockFs::storage` and `shared::MockFs::unmounted` are already in place from prior migrations). No `mount.rs` fixture changes (every helper unlock reuses is already exported).

## Existing functions / utilities reused

- `shared::MockFs::storage` (`test_fixtures/shared.rs:48-55`) -- already implements `Filesystem` with the canonical `/mnt/storage` mountinfo body byte-identical to unlock's `MOUNTINFO_BTRFS`. The unlock fixture wraps it via `unlock_storage_fs(paths: &[&str])` for ergonomic per-test calls. Mirrors `mount::mount_fs` (`test_fixtures/mount.rs:51`).
- `shared::MockFs::unmounted` (`test_fixtures/shared.rs:60-66`) -- already implements `Filesystem` with the rootfs-only mountinfo body byte-identical to unlock's `MOUNTINFO_WITHOUT_TARGET`. Reused via the existing facade-exported `mount::mount_fs(paths: &[&str])`. No new wrapper.
- `mount::base_two_disk_runner()` (`test_fixtures/mount.rs:256`, re-exported at `test_fixtures.rs:98-104`) -- canonical 2-disk preflight runner: `MountpointCheck` not-mounted, `CryptsetupLuksUuid` x2, `with_luks_dump_text_luks2` x2, `with_mappers_closed`, `CryptsetupTestPassphrase` x2 verify-pass with `MOUNT_TEST_PASSPHRASE_BYTES`. Per-test override semantics on `with_output_stdin` / `with_output` are pinned by `mock_runner_with_output_stdin_override_after_base_wins` (cited in `mount.rs:253-255`); unlock's `passphrase_mismatch_names_failing_disk` and `plan_unlock_note_only_success_when_already_mounted` rely on those semantics.
- `mount::test_config()` (`test_fixtures/mount.rs:181`, re-exported) -- byte-identical to unlock's local `three_disk_config`. Both call `Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()`.
- `mount::two_disk_membership()` / `mount::three_disk_membership()` (`test_fixtures/mount.rs:191`, `205`, re-exported) -- byte-identical to unlock's local `three_disk_membership` and to the inline 2-disk membership constructions in `passphrase_mismatch_names_failing_disk`, `cmd_unlock_preserves_mount_error_when_cleanup_close_fails`, and the dry-run-rendering tests.
- `mount::luks_uuid_ok(device, uuid)` (`test_fixtures/mount.rs:94`, re-exported) -- byte-identical to the inline `with_output(CryptsetupLuksUuid { device }, RawCommandOutput { stdout: "<uuid>\n", ... })` blocks throughout unlock.
- `mount::test_passphrase_fail(device)` (`test_fixtures/mount.rs:108`, re-exported) -- byte-identical to the inline `err_raw("cryptsetup open --test-passphrase", 2, "No key available with this passphrase.")` used by `passphrase_mismatch_names_failing_disk` for disk2's verify failure.
- `mount::MOUNT_TEST_PASSPHRASE_BYTES` (`test_fixtures/mount.rs:179`, re-exported) -- the `b"testpass"` constant. Replaces every `b"testpass".to_vec()` / `b"testpass"` literal at unlock's `with_output_stdin` call sites.
- `mount::ok_raw` / `mount::err_raw` (`test_fixtures/mount.rs:72`, `81`, re-exported) -- byte-identical to unlock's locals. Aliased on import (mandatory; same Rust duplicate-definition rule that motivated `enroll_err_raw`).
- `doctor::isolated_paths()` (`test_fixtures/doctor.rs:26`, re-exported at `test_fixtures.rs:84-90`) -- byte-identical to unlock's local `test_paths`. Returns `(TempDir, StatePaths)`.
- `cmd::MockRunner::with_output` / `with_output_stdin` / `with_luks_dump_text_luks2_for` / `with_mappers_closed` / `with_mapper_open` (`cmd.rs:988`, `1004`, `1126`, `1138`, `1150`-style) -- the canonical chaining surface; `unlock_with_test_passphrase_ok` / `unlock_with_open_mapper_ok` / `unlock_with_mount_ok` / `unlock_with_mount_degraded_ok` / `unlock_with_three_mappers_open` are single compositions over these.
- `cmd::MockRunner::with_handler` (`cmd.rs:1021`-style) -- exists, but **deliberately not used** by the unlock fixture (a broad handler would resolve the `Mount` request when only `MountWithOptions` was seeded, silently masking the regression `unlock_bricked_disk_uses_degraded_mount` exists to catch).
- `MockRunner::with_output_stdin`'s `HashMap` insert / overwrite behavior (`cmd.rs:1004-1014`) -- already pinned by `mock_runner_with_output_stdin_override_after_base_wins`. Unlock's `passphrase_mismatch_names_failing_disk` exercises the override path on top of `base_two_disk_runner`; the existing pin still applies and no new `cmd.rs` test is required.

## Out of scope for this plan

- Touching `cli/src/unlock.rs` production code (lines 1-235). This is a pure test-side refactor.
- Migrating other command modules (`add.rs`, `replace.rs`, `recover.rs`, etc.) -- unlock is one migration target; siblings come in follow-up plans.
- Building an `UnlockTopology` / `UnlockPool` handler installer or `UnlockParamsBuilder` (rejected in §A; the Mount-vs-MountWithOptions invariant + per-test request-set diversity make broad handlers and builders the wrong shape).
- Promoting `unlock_with_test_passphrase_ok` / `unlock_with_open_mapper_ok` / `unlock_with_mount_ok` / `unlock_with_mount_degraded_ok` to `shared`. Each has only one in-tree consumer scope today (the unlock test mod). If `mount` ever grows tests that want the same chain shape, promote then.
- Promoting `unlock_storage_fs(&[&str])` to `shared`. The thin `&[&str]` wrapper is a per-scope ergonomics helper; mount's mirror at `mount.rs:51` is also per-scope. Promote when a third scope adopts it.
- Stripping the `unlock_` prefix from helpers whose names don't directly collide today (e.g. `unlock_btrfs_balance_status_paused`). The prefix is applied uniformly across the new module's exports for two reasons: (a) it removes the staged-migration duplicate-definition hazard for every helper, even ones whose name happens not to collide at the facade today; (b) it gives the scope a recognisable shared shape so a `grep unlock_` walks the entire fixture surface.
- Adding a new `cmd.rs` regression test. `mock_runner_with_output_stdin_override_after_base_wins` already pins the override-wins contract that `passphrase_mismatch_names_failing_disk` and `plan_unlock_note_only_success_when_already_mounted` rely on; unlock does not introduce a new dispatch path.
- Promoting the `unlock_passphrase_file()` helper to `shared`. Each scope writes the passphrase file with different bytes (mount: `MOUNT_TEST_PASSPHRASE_BYTES`; enroll: `enroll_make_existing_keyfile` writes `KEYFILE_SIZE` zero bytes; unlock: `MOUNT_TEST_PASSPHRASE_BYTES`); the helper is more useful per-scope than as a single shared one.
- Migrating the bogus-passphrase-path inline construction in `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`. The bogus path IS the load-bearing assertion (proves credential resolution didn't run). A fixture-default passphrase file would invite a "tidy" that defeats the test.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | Swapping the local `MockFs` (which returns `Err(NotFound)` for any path other than `/proc/self/mountinfo`) for `shared::MockFs::storage` (which additionally returns `"none\n"` for `*/exclusive_operation`) silently changes a test's behavior if any production path on the unlock call graph reads `*/exclusive_operation`. | Sub-commit 1's call-graph audit (Verification) explicitly enumerates every `fs.read_to_string` / `fs.is_block_device` / `fs.list_dir` site reachable from `cmd_unlock` across `cli/src/{unlock,mount,mount_check,probe,credential,credential_verify,membership}.rs` -- including `mount_check.rs`, where `is_btrfs_mounted` (`mount_check.rs:162-168`) and `fstype_at_mount_via_fs` (`mount_check.rs:172-178`) read `MOUNTINFO_PATH` through the trait. The accepted reads are `/proc/self/mountinfo` and path-existence probes; today's audit clears with no `*/exclusive_operation` reads on the unlock call graph (excl_op reads live in preflight balance/scrub/replace paths, none of which `cmd_unlock` reaches). If a forbidden read surfaces, the fixture rebuilds `MockFs` scope-local (replicating today's `unlock.rs:259-296`) and the rest of the plan is unchanged -- the load-bearing rule "abort to scope-local `MockFs` if any other read appears" survives the verification step. |
| 2 | The Mount-vs-MountWithOptions enum-variant assertion in `unlock_bricked_disk_uses_degraded_mount` silently inverts because the new `unlock_with_mount_degraded_ok` helper accidentally seeds plain `Mount` (or both variants). | The helper's body is a single `runner.with_output(MountWithOptions { device, mount_point, options: vec!["degraded".to_owned()] }, ok_raw("mount -o noatime,skip_balance,degraded"))` -- byte-identical to `unlock.rs:435-442` and verified by `git diff` in the new module. The helper name explicitly contains `degraded`; a sibling helper `unlock_with_mount_ok` exists for the plain-Mount case so a future contributor cannot "tidy" them into one. Sub-commit 2's verification step explicitly mutates the helper to `Mount { ... }` locally (in a scratch commit, then reverted) and confirms the test fails with `MissingMock`. |
| 3 | Promoting `unlock_with_three_mappers_open(runner)` masks a future regression where one of the canonical UUIDs / backing devices changes in one consumer but not the other (the all-mappers-already-open classification depends on UUID/path equality across `cryptsetup status` outputs). | The promoted helper preserves the exact set of seeded UUIDs and backing devices from the inline forms verbatim: `("braid-disk1", "/dev/vda", "aaaaaaaa-1111-2222-3333-444444444444")`, `("braid-disk2", "/dev/vdb", "bbbbbbbb-...")`, `("braid-disk3", "/dev/vdc", "cccccccc-...")`. Add a one-paragraph doc comment listing the canonical values so a future contributor who needs to vary them reads the rationale and decides "this isn't actually generic" before adding parameters. |
| 4 | The bogus-passphrase-path invariant in `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` gets "tidied" into using `unlock_passphrase_file()` during sub-commit 4 because the helper looks like the right thing. | Sub-commit 4's table row explicitly enumerates the keep-inline list: `let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase"); ... passphrase_file: Some(&bogus)` (`unlock.rs:1889`). The risk row here flags the invariant. Plus the test's preamble at `unlock.rs:1770-1777` describes the invariant in prose; sub-commit 4 normalizes the preamble to the canonical `// Intent / Why it exists / Scenario` line-comment form (the existing wording is `///` doc-comment, which the migration converts) while preserving the substantive content. Run-time signal: a regression that uses the helper would fail `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` with a real I/O error or `MissingMock`, not silent success. |
| 5 | Migration accidentally drops or garbles a test preamble during a rewrite. The existing preambles in `unlock.rs::tests` are mixed: some are `///` doc-comments (e.g. `unlock.rs:335-341`, `488-492`, `626-637`, `960-966`, `1770-1777`), one is a `/* */` block comment (`1124-1136`), and one is the canonical `//` line-comment form (`783-788`). | AGENTS.md's "Test Conventions" section and `docs/testing.md:11-22` mandate the canonical `// Intent / Why it exists / Scenario` line-comment form. Each touched test's preamble is **converted** to the canonical form during its sub-commit rewrite, preserving the substantive content (intent / risk / scenario) verbatim while normalising the comment style. The conversion is a small mechanical cleanup -- not a separate sub-commit -- and lives in the same diff as the helper-swap rewrite. Verification (per sub-commit): `git log -p cli/src/unlock.rs` -- the diff for each migrated test must show (a) the canonical `// Intent` / `// Why it exists` / `// Scenario` triple immediately above `#[test]`, (b) the substantive content matching the prior preamble's claims, (c) helper-call and import swaps in the body. Tests not touched by a sub-commit keep their existing preamble style; sub-commit 6 does not retroactively normalise un-migrated tests. |
| 6 | The mountinfo body distinction (`MOUNTINFO_BTRFS` vs `MOUNTINFO_WITHOUT_TARGET`) gets confused during sub-commit 4 -- a test that needs the rootfs-only body picks `unlock_storage_fs` instead of `mount_fs` and silently passes because the post-mount probe coincidentally still reports the right thing for that scenario. | Sub-commit 4 explicitly enumerates which test uses which fs helper: `unlock_warns_on_paused_balance` -> `unlock_storage_fs`; `unlock_tolerates_post_mount_probe_mounted_false` -> `mount_fs`; `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` -> `mount_fs`. The two `mount_fs` consumers carry inline comments referencing the load-bearing behavior. Run-time signal: swapping `mount_fs` for `unlock_storage_fs` in `unlock_tolerates_post_mount_probe_mounted_false` would flip the post-mount probe to `mounted=true`, which would in turn enrich pool.json and break the `assert_eq!(disk.mounted, None)` assertion. |
| 7 | A reviewer reads the `unlock_*` prefix on every helper (and the `ok_raw as unlock_ok_raw` / `err_raw as unlock_err_raw` import aliases) and decides to "simplify" the names back to bare forms because mount and doctor mostly ship unprefixed names. | The prefix and the aliases are load-bearing for the same two reasons captured at the top of the new module's doc comment AND in inline comments alongside the alias `use` lines: (a) facade collisions with `mount::ok_raw` / `mount::err_raw`; (b) the staged migration's same-module `use` + local `fn` duplicate-definition error. The module-level doc comment quotes these constraints so a future reviewer doesn't try a sweep without re-deriving the rationale. The `enroll_*`-prefixed migration set the precedent and the same wording can be reused. |
| 8 | The paused-balance literal stdout body in `unlock_btrfs_balance_status_paused` drifts from the body `status::get_balance_report` parses as "paused", silently downgrading the warning to nothing while `unlock_warns_on_paused_balance` (which only asserts `Ok(())`) keeps passing. | Sub-commit 1's "Pin the paused-balance literal" task copies the body byte-for-byte from `unlock.rs:1082-1090` into the helper, AND lands the focused self-test `unlock_btrfs_balance_status_paused_classifies_as_paused` that pipes the helper through `status::emit_paused_balance_warning` and uses a full-string `assert_eq!(output, expected)` against the canonical warning text from `status.rs:697-700` (modeled on `status.rs::tests::emit_paused_balance_warning_writes_to_buffer` at `status.rs:2422-2428`). The full-string equality (not `contains` substrings) catches extra blank lines, missing leading newline, drifted hint phrasing, and any other byte-level deviation. The self-test, not `unlock_warns_on_paused_balance`, is the regression signal: a literal drift surfaces as `assert!(warned)` failing or the `assert_eq!` failing, regardless of how `cmd_unlock`'s success path is asserted in sub-commit 4. |
| 9 | The `mount::base_two_disk_runner` reuse in sub-commits 3 and 5 changes the `runner.requests()` log indices because of an extra preflight seed (e.g. `with_luks_dump_text_luks2`) the original inline runners didn't have. Tests that don't pin `runner.requests()` ordering pass, but a future test that does pin it (or a future regression that relies on the `MissingMock` path being the first failure) silently changes behavior. | `mount::base_two_disk_runner`'s seed list is documented at `mount.rs:232-291` and matches what every 2-disk unlock test today requires. None of unlock's 11 tests assert `runner.requests() == vec![...]`; only `runner.requests().iter().any(...)` (used by `cmd_unlock_preserves_mount_error_when_cleanup_close_fails` for the cleanup-close assertion). Adding extra `MockRunner` seeds only affects the `outputs` map, not the `requests()` log; `MockRunner::run` logs only what production calls (`cmd.rs:1172-1175`). Behavior is preserved by construction. |

## Verification

End-to-end gate: `just test-rust` is green at every sub-commit boundary. `just test-rust` (Justfile) runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard` as a fixed command. Filtered runs go through `cargo test` directly.

**Pre-sub-commit-1 call-graph audit (one-time):**

```
grep -nE "fs\.(read_to_string|is_block_device|list_dir)" cli/src/{unlock,mount,mount_check,probe,credential,credential_verify,membership}.rs
```

The file set is the call graph reachable from `cmd_unlock`: `unlock.rs` (entry), `mount.rs` (planning + execute), `mount_check.rs` (mountinfo reads via the trait -- `is_btrfs_mounted` at `mount_check.rs:162-168` and `fstype_at_mount_via_fs` at `mount_check.rs:172-178`; missing this file in the earlier grep would have under-counted the relevant reads), `probe.rs` (`probe_pool` post-mount enrichment, called from `unlock.rs:138`), `credential.rs` / `credential_verify.rs` (resolve / verify paths), `membership.rs` (`refresh_pool_metadata`).

Triage the matches against the accepted-read list:

- `fs.read_to_string("/proc/self/mountinfo")` -- accepted; `shared::MockFs::storage` and `shared::MockFs::unmounted` answer with the bytes byte-identical to today's local `MOUNTINFO_BTRFS` and `MOUNTINFO_WITHOUT_TARGET`, respectively.
- `fs.exists(...)` for device / mapper paths -- accepted; both `MockFs` variants answer from the seeded `paths` list.
- Anything else -- in particular `*/exclusive_operation`, `*/sys/...`, `/etc/...` -- aborts the swap to `shared::MockFs::storage` and the migration ships a scope-local `MockFs` (replicating today's `unlock.rs:259-296`) instead.

If the triage clears, sub-commit 1 proceeds. If a forbidden read surfaces, the fixture's filesystem helper is rebuilt scope-local and the rest of the plan is unchanged.

**Per sub-commit:**

- **Sub-commit 1** (scaffolding): `cargo check --manifest-path cli/Cargo.toml --tests` clean (no `unused_imports` / `dead_code` errors -- the `#[allow(...)]` annotations cover the staggered consumer rollout). Then `just test-rust` green. Confirm the paused-balance literal in `unlock_btrfs_balance_status_paused` matches `unlock.rs:968-1130` byte-for-byte (`diff <(grep -A20 "fn unlock_btrfs_balance_status_paused" cli/src/test_fixtures/unlock.rs) <(grep -A20 "BtrfsBalanceStatus" cli/src/unlock.rs | grep -A5 "paused")`).
- **Sub-commit 2** (bricked-disk family, 2 tests, helper-call migration):
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_bricked_disk_uses_degraded_mount`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_bricked_disk_refuses_without_flag`
  Then `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests`. Then `just test-rust`. The Mount-vs-MountWithOptions distinction must round-trip: a scratch commit that flips `unlock_with_mount_degraded_ok` to seed plain `Mount` must surface `MissingMock` in `unlock_bricked_disk_uses_degraded_mount`; absence of any mount mock must surface `DegradedRefused` in `unlock_bricked_disk_refuses_without_flag`. Revert the scratch commit before landing.
- **Sub-commit 3** (2-disk full-execution, 2 tests, on top of `base_two_disk_runner`):
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::passphrase_mismatch_names_failing_disk`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::cmd_unlock_preserves_mount_error_when_cleanup_close_fails`
  Then `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests`. Then `just test-rust`. The disk2-named-error contract (msg contains "disk2", does NOT contain "disk1", does NOT contain "Wrong passphrase?") must round-trip; the cleanup-keeps-trying contract (`runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupClose { mapper: m }) if m == "braid-disk2")`) must round-trip. The override-wins behavior on `base_two_disk_runner` (disk2 verify-fail layered on disk2 verify-ok) must produce the rejection, not the success -- regression-pinned by `mock_runner_with_output_stdin_override_after_base_wins`.
- **Sub-commit 4** (3-disk family, 3 tests):
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_warns_on_paused_balance`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::unlock_tolerates_post_mount_probe_mounted_false`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`
  Then `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests`. Then `just test-rust`. `unlock_warns_on_paused_balance` is the success-path test -- its only assertion is `result.expect("unlock should succeed even with paused balance")` (`unlock.rs:1120-1121`); the body-to-warning contract is pinned by the sub-commit-1 self-test `unlock_btrfs_balance_status_paused_classifies_as_paused`, which runs as part of `unlock::tests` here. The post-mount probe `mounted=false` -> pool.json fields stay `None` assertion must round-trip. The bogus-passphrase-file invariant must hold: the sentinel `let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase"); ... passphrase_file: Some(&bogus)` stays inline; a regression that called `resolve_credential` here would surface a real I/O error, not `MissingMock`.
- **Sub-commit 5** (dry-run preview rendering, 4 tests):
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::plan_unlock_dry_run_render_2_closed_disks`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::plan_unlock_dry_run_render_2_closed_disks_with_key_file`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::plan_unlock_note_only_success_when_already_mounted`
  - `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests::plan_unlock_preserves_notes_on_degraded_refused`
  Then `cargo test --manifest-path cli/Cargo.toml --lib unlock::tests`. Then `just test-rust`. The keyfile preview-string substring assertions (`"--key-file /run/keys/braid.key --keyfile-size 4096"` present, `"--key-file=-"` absent) must round-trip; the `assert_eq!(rendered, "pool already mounted at /mnt/storage\n")` must round-trip exactly; the note ordering assertions (Ok/Ok/Skip in that order on the `Err` arm) must round-trip.
- **Sub-commit 6** (cleanup): `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references and no `unused_imports` / `dead_code` warnings. `cargo build --manifest-path cli/Cargo.toml --tests` clean. `just test-rust` full suite green. The `#[allow(dead_code)]` annotations on `test_fixtures::unlock` items are removed and `cargo build` still clean.

**Behavior-preservation check (mechanical, all sub-commits):**

- Each touched test's preamble is normalised to the canonical `// Intent / Why it exists / Scenario` line-comment form per `docs/testing.md:11-22`, with the substantive content (intent / risk / scenario claims) preserved verbatim. `git log -p cli/src/unlock.rs` per sub-commit -- the diff for each migrated test shows the preamble in canonical form plus helper-call swaps in the body. Untouched tests keep their existing preamble style.
- Every `assert!(...)` / `assert_eq!(...)` body is unchanged across the migration -- the migration touches setup code (runner, fs, params, by-id, passphrase construction) only.
- Every `runner.requests().iter().any(...)` assertion observes the same request log, since the migration does not introduce `with_handler` and `MockRunner::run` always logs.
- Every `// If the code incorrectly uses Mount instead of MountWithOptions` comment (483-485) and analogous Mount-vs-MountWithOptions intent comment is preserved.
- The bogus-passphrase-file sentinel in `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` is preserved verbatim (`std::path::PathBuf::from("/definitely/not/a/real/path/passphrase")` at `unlock.rs:1889`) -- not replaced by `unlock_passphrase_file()`.
- The mountinfo body choice per test is preserved: tests using `unlock_storage_fs` get the storage body; tests using `mount_fs` get the rootfs-only body.

No new VM tests, no parser-fixture refresh, no production behavior change. The existing test suite IS the verification.

## Branch and commit shape

Work on a feature branch (e.g. `refactor-unlock-test-fixtures`). Each numbered sub-commit above is one git commit. PR opens once sub-commit 6 lands. Reviewer can walk the branch commit-by-commit; each commit is independently green.

Conventional Commits-style messages (lowercase first word per AGENTS.md):

- `refactor(test): add unlock scope test fixture module` (sub-commit 1)
- `refactor(unlock): migrate bricked-disk mount-variant tests to shared fixtures` (sub-commit 2)
- `refactor(unlock): migrate 2-disk full-execution tests to shared fixtures` (sub-commit 3)
- `refactor(unlock): migrate 3-disk post-mount tests to shared fixtures` (sub-commit 4)
- `refactor(unlock): migrate dry-run preview tests to shared fixtures` (sub-commit 5)
- `refactor(unlock): drop migrated locals from unlock tests module` (sub-commit 6)
