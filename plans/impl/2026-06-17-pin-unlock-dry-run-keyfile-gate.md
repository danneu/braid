# Plan: pin the dry-run/keyfile-validation boundary in `cmd_unlock`

## Context

`braid unlock --dry-run --key-file <path>` must render the preview without
validating (stat-ing) the operator-supplied keyfile. Keyfile validation
(`luks::validate_user_keyfile_path`, `cli/src/luks.rs#validate_user_keyfile_path`)
runs only inside `credential::resolve_credential`
(`cli/src/credential.rs#resolve_credential`), which `cmd_unlock` calls only inside
`UnlockPlan::execute` -- strictly after the dry-run gate
(`cli/src/unlock.rs`, `if params.dry_run { plan.preview().print_colored(); return Ok(()); }`).
The planning path (`plan_unlock` -> `mount::compile_open_steps`) only renders the
keyfile path string; it never stats it. This is the intended behavior under
ADR 022's plan/execute separation (`docs/design/decisions/022-dry-run-preview-model.md`):
credential resolution is an execute-time concern, so preview must still succeed
with a bad keyfile.

There is a passphrase test for this gate
(`cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`,
`cli/src/unlock.rs`) but **no keyfile analog at the `cmd_unlock` entrypoint**.
The passphrase test runs with `key_file: None`, so it never exercises the keyfile
branch of `resolve_credential`. The only keyfile dry-run coverage today
(`plan_unlock_dry_run_render_2_closed_disks_with_key_file`) calls `plan_unlock`
directly and proves non-validation only incidentally, as a side effect of a render
test -- it cannot catch a regression in `cmd_unlock`'s own gate ordering for the
keyfile branch.

This plan closes that narrow but real gap by adding the symmetric sibling test.
It is the symmetric completion of an existing, proven pattern -- no production
change, no new abstraction.

## The change

Add **one** unit test to the tests module in `cli/src/unlock.rs`, placed
immediately after the passphrase analog
(`cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`, currently
the last test in the file).

### Test

- **Name:** `cmd_unlock_dry_run_skips_keyfile_validation_with_disks_to_unlock`
  (mirrors the passphrase sibling; "keyfile validation" names the exact stat seam
  this guards and distinguishes it from its twin at a glance).
- **Setup:** identical to the passphrase analog -- `isolated_paths()`,
  `test_config()`, `two_disk_membership()`, `unlock_storage_fs(&[disk1, disk2])`,
  `base_two_disk_runner()` (a closed 2-disk pool, so `to_unlock` is non-empty and
  the gate is actually reachable). `UnlockParams` with `dry_run: true`,
  `allow_degraded: false`, `NoopSleeper`, the mock virtio backing-path resolver.
  **Bind the tempdir guard** -- `let (state_dir, sp) = isolated_paths();` (not the
  template's discarded `_state_dir`), because this test derives the bogus keyfile
  path from `state_dir.path()` and the `TempDir` must stay live for the test's
  lifetime. `isolated_paths()` returns `(tempfile::TempDir, StatePaths)`
  (`cli/src/test_fixtures/doctor.rs#isolated_paths`); the `TempDir` exposes `.path()`.
- **Credential fields (the only divergence from the passphrase test):**
  `passphrase_file: None`, `key_file: Some(&bogus)` where
  `let bogus = state_dir.path().join("missing-braid.key");`. Anchor the bogus path
  under the freshly-created, empty `isolated_paths()` tempdir -- a child we never
  write is guaranteed absent **independent of global filesystem state**. Do *not*
  use a literal absolute path (e.g. `/definitely/not/a/real/path/...`): keyfile
  validation bypasses the `F: Filesystem` seam and calls `std::fs::symlink_metadata`
  on the real host outside `runner.requests()`, so if that literal path happened to
  exist as a valid 4096-byte file the `Ok(())` witness would be silently defeated
  and the runner denylist could not catch it. A genuinely-absent **nonexistent**
  path (not a wrong-size temp file) is the clean witness; anchoring it in the live
  tempdir makes that absence self-evident. (This deliberately uses a stronger
  witness than the passphrase sibling's literal `/definitely/not/a/real/path/passphrase`;
  hardening that pre-existing sibling is out of scope for this plan.)

### Assertions

1. **Primary proof:** `result.expect("dry-run with disks to unlock must render the
   preview and return without validating the (nonexistent) key file")`.
   `Ok(())` is the load-bearing assertion: if dry-run regressed and resolved the
   credential, `validate_user_keyfile_path` would stat the bogus path and return
   `Err(LuksError::Validation("keyfile not found: ..."))` before `Ok(())`.
2. **Defense-in-depth:** copy the passphrase test's zero-execute-commands check
   verbatim -- filter `runner.requests()` against the 7-variant `matches!` denylist
   (`CryptsetupTestPassphrase | CryptsetupTestKeyFile | CryptsetupLuksOpen |
   CryptsetupLuksOpenKeyFile | BtrfsDeviceScanAll | Mount | MountWithOptions`) and
   assert `executed.is_empty()`. Keep the inherited "future-proof + self-documenting"
   comment.

### Preamble (required `Intent / Why it exists / Scenario`)

Make the `// Why it exists:` line precise about which assertion proves what: a
consulted keyfile stat never appears in `runner.requests()` (it is not a runner
command), so **`Ok(())` is the proof the stat was skipped**; the denylist is the
defense-in-depth backstop against the broader execute path running, not the witness
for keyfile validation. Note the complementary coverage: render content is pinned
at the `plan_unlock` level by `plan_unlock_dry_run_render_2_closed_disks_with_key_file`;
this test pins `cmd_unlock`'s gate ordering for the keyfile branch.

## Explicitly out of scope (considered, rejected)

- **No shared helper / parameterized test.** The codebase deliberately copies whole
  test bodies one-scenario-per-`#[test]` with distinct preambles (e.g.
  `plan_unlock_dry_run_render_2_closed_disks` and its `_with_key_file` twin are ~95%
  identical, no helper). A plain sibling copy matches project style; the per-test
  preamble cannot live in a helper anyway.
- **No routing `validate_user_keyfile_path` through the `Filesystem` seam.** That
  would change a trait, `resolve_credential`'s signature, and every caller (recover,
  enroll-key-file) for a Low-severity test gap -- and would alter the very behavior
  under test. The direct-`std::fs` + real-nonexistent-path approach exercises the
  production path as written and needs zero production changes.
- **No render-content assertion through `cmd_unlock`.** `cmd_unlock` prints the
  preview to stdout via `print_colored()` and returns `()`; the rendered string is
  not reachable from its return value and the test does not capture stdout.
  `Ok(())` + zero-execute-commands is the complete, honest proof set at this
  entrypoint.

## Files

- `cli/src/unlock.rs` -- add the one test (only file changed). Template:
  `cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`; render
  sibling for the preamble cross-reference:
  `plan_unlock_dry_run_render_2_closed_disks_with_key_file`. All fixtures and
  imports are already in scope (`base_two_disk_runner`, `two_disk_membership`,
  `unlock_storage_fs`, `test_config`, `isolated_paths`, `CmdRequest` at the tests-mod
  `use` block; `Path` via `use super::*`). No new fixtures or imports.

## Verification

- `cargo test -p braid-cli cmd_unlock_dry_run_skips_keyfile_validation` -- new test
  passes.
- Confirm it fails for the right reason if the gate regresses: temporarily hoist
  `resolve_credential(...)` (keyfile branch) above the `if params.dry_run` return in
  `cmd_unlock`, re-run -- the test must fail at `result.expect(...)` with the
  "keyfile not found" validation error (proving `Ok(())` is the real witness).
  Revert.
- `just test-rust` (or `cargo test -p braid-cli`) -- full crate suite stays green.
- ASCII-only check on touched output: no user-facing strings added, but run
  `scripts/docs/check-output-ascii.py` if convenient (the test adds only comments
  and assertion messages, which are exempt, so this is a formality).

## Follow Up

- Harden the passphrase sibling's witness. `cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`
  (`cli/src/unlock.rs`) uses a literal `/definitely/not/a/real/path/passphrase`;
  unlike the new keyfile test it does not anchor the bogus path under an
  `isolated_paths()` tempdir, so its absence depends on global filesystem state.
  Passphrase resolution reads the file lazily (not at `resolve_credential` time),
  so the current risk is lower than the keyfile stat seam, but anchoring it in a
  freshly-created tempdir child would make the absence self-evident and match the
  new sibling. Explicitly deferred by this plan as out of scope.
