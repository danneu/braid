# Plan: pin that `cmd_unlock --dry-run` never resolves the credential when there are disks to unlock

## Context

ADR 022 (`docs/design/decisions/022-dry-run-preview-model.md`) requires dry-run
preview to be side-effect free: only "side-effect-free probes" run while building
a preview, and the lone documented exception (recover) resolves credentials only
behind a `!dry_run` gate. For `braid unlock`, that contract lives in the dry-run
gate of `cmd_unlock`:

```rust
// cli/src/unlock.rs:252-258
if params.dry_run {
    plan.preview().print_colored();
    return Ok(());
}
plan.execute(runner, fs, params)   // resolve_credential lives downstream, here
```

`resolve_credential` is reached only inside `UnlockPlan::execute`
(`cli/src/unlock.rs:104-110`) and only when `plan.to_unlock` is non-empty. So a
refactor that hoisted `resolve_credential` above the `if params.dry_run` gate
would make dry-run start reading the passphrase/keyfile -- a silent ADR 022
violation.

**The gap:** no test exercises this. A repo-wide search confirms **no caller of
`cmd_unlock(...)` anywhere sets `dry_run: true`**. The two existing dry-run tests
(`plan_unlock_dry_run_render_2_closed_disks` at `unlock.rs:744`,
`..._with_key_file` at `:803`) call `plan_unlock` **directly, not `cmd_unlock`**,
and `plan_unlock` structurally never resolves a credential -- so they cannot
catch a regression in `cmd_unlock`'s gate ordering. The sibling
`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (`:1512`) proves
the *empty-`to_unlock`* skip (a different branch), not the dry-run skip.

**Outcome:** add one Rust unit test that runs `cmd_unlock` with `dry_run: true`,
a non-empty `to_unlock`, and a credential source that would error if read --
pinning the dry-run-skips-credential invariant against future refactors.

## Change

Single test file: **`cli/src/unlock.rs`** (the `#[cfg(test)] mod tests` block).
No production code changes.

Add one test, placed immediately after
`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` (ends ~`:1589`)
so the two "credential-skip" siblings sit together. It clones the setup of
`plan_unlock_dry_run_render_2_closed_disks` but calls `cmd_unlock` (not
`plan_unlock`) and swaps the harmless `passphrase_file: None` for the bogus
nonexistent-path sentinel already used at `unlock.rs:1566`.

### Reused fixtures/helpers (all already in scope in the tests module)

- `two_disk_membership()` -- `test_fixtures/mount.rs:190` (disk1 + disk2).
- `base_two_disk_runner()` -- `test_fixtures/mount.rs:275`; encodes
  mountpoint = not-mounted and both mappers closed, so `plan_unlock` yields an
  `OpenPlan` with `to_unlock = [disk1, disk2]` (non-empty). No extra mocks
  needed -- the existing render test proves this runner alone produces the full
  open/scan/mount preview, and the dry-run gate returns before `execute`, so
  none of those commands are issued.
- `unlock_storage_fs(&[...])`, `isolated_paths()`, `test_config()`,
  `crate::progress::NoopSleeper`,
  `crate::test_fixtures::mock_virtio_backing_path_resolver()` -- same as the
  existing dry-run render test.
- Bogus sentinel: `std::path::PathBuf::from("/definitely/not/a/real/path/passphrase")`
  (mirrors `unlock.rs:1566`). Contrast with `unlock_passphrase_file()`
  (`test_fixtures/unlock.rs:199`), which returns a *real* tempfile -- we
  deliberately want a path that does not exist so any read attempt errors.

### Test sketch

```rust
// Intent: `cmd_unlock --dry-run` must render the preview and return WITHOUT
//   resolving the unlock credential, even when there ARE disks to unlock.
// Why it exists: the dry-run gate in cmd_unlock returns before plan.execute,
//   the only path that calls resolve_credential. A refactor hoisting
//   resolve_credential above the `if params.dry_run` gate would make dry-run
//   read the passphrase -- violating ADR 022's side-effect-free preview
//   contract. The sibling cmd_unlock_skips_credential_resolution_when_nothing_
//   to_unlock pins the empty-to_unlock skip, not the dry-run skip; the two
//   plan_unlock_dry_run_render_* tests call plan_unlock directly (which never
//   resolves a credential), so neither catches this regression.
// Scenario: operator runs `braid unlock --dry-run --passphrase-file <path>`
//   against a 2-disk closed pool (both mappers closed -> to_unlock non-empty)
//   where the passphrase file does not exist.
#[test]
fn cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock() {
    let (_state_dir, sp) = isolated_paths();
    let config = test_config();
    let membership = two_disk_membership();
    let fs = unlock_storage_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]);
    let runner = base_two_disk_runner();

    // Path does not exist: if dry-run regresses and resolves a credential,
    // read_passphrase opens the bogus path and fails before Ok(()).
    let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");

    let result = cmd_unlock(
        &runner,
        &fs,
        &UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: Some(&bogus),
            key_file: None,
            allow_degraded: false,
            dry_run: true,
            sleeper: &crate::progress::NoopSleeper,
            backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
        },
    );

    result.expect(
        "dry-run with disks to unlock must render the preview and return \
         without reading the (nonexistent) passphrase file",
    );

    // Future-proof + self-documenting: even if base_two_disk_runner ever
    // gains open/mount mocks (removing the implicit missing-mock backstop),
    // dry-run must still issue ZERO execute-only commands. This denylist is
    // the complete set the unlock execute path can run (credential verify +
    // LUKS open + scan + mount, both passphrase and keyfile); none are issued
    // by plan_unlock's probe, so any hit means execute wrongly ran.
    let executed: Vec<CmdRequest> = runner
        .requests()
        .into_iter()
        .filter(|r| {
            matches!(
                r,
                CmdRequest::CryptsetupTestPassphrase { .. }
                    | CmdRequest::CryptsetupTestKeyFile { .. }
                    | CmdRequest::CryptsetupLuksOpen { .. }
                    | CmdRequest::CryptsetupLuksOpenKeyFile { .. }
                    | CmdRequest::BtrfsDeviceScanAll
                    | CmdRequest::Mount { .. }
                    | CmdRequest::MountWithOptions { .. }
            )
        })
        .collect();
    assert!(
        executed.is_empty(),
        "dry-run must issue zero execute-only commands, got: {executed:?}",
    );
}
```

### Assertion rationale

- **`result.expect(...)` (primary).** With `to_unlock` non-empty, the only way
  to reach `Ok(())` is the dry-run gate returning before `execute`. If
  `resolve_credential` were hoisted above the gate, the bogus path makes
  `luks::read_passphrase` error -> `cmd_unlock` returns `Err` -> test fails.
- **Zero execute-only commands (secondary, future-proof).** Asserts dry-run
  issued none of the commands the unlock execute path runs. The probe-only
  `base_two_disk_runner()` already makes an accidental fall-through to `execute`
  fail via missing mocks, but that backstop is implicit and would vanish if the
  shared fixture later gains open/mount mocks; the explicit denylist survives
  that and documents intent. The **complete** execute-only set, traced through
  `mount::execute_unlock_and_mount` -> `open_disks_with_credential` +
  `scan_and_mount` (and `execute_mount_only`, `mount.rs:509`/`633`/`797`), is:
  `CryptsetupTestPassphrase`, `CryptsetupTestKeyFile`, `CryptsetupLuksOpen`,
  `CryptsetupLuksOpenKeyFile`, `BtrfsDeviceScanAll`, `Mount`, `MountWithOptions`
  (all verified against the `CmdRequest` enum, `cmd.rs:21`). Keyfile and
  degraded-mount (`MountWithOptions`) variants are denied defensively even
  though this test drives the passphrase, non-degraded source. None of the
  seven overlap the probe path (`MountpointCheck`, `CryptsetupLuksUuid`,
  `CryptsetupLuksDumpText`, `CryptsetupStatus`), so the denylist cannot
  false-positive on legitimate dry-run probing. Uses `MockRunner::requests()`
  (`cmd.rs:1477`), already used at `unlock.rs:618`.

### Deliberately NOT asserted / out of scope

- **"Preview rendered" text.** `cmd_unlock`'s dry-run branch calls
  `plan.preview().print_colored()`, which writes to real stdout via `print!`
  with no injectable writer seam (`preview.rs:300`). Capturing it would require
  an OS-level stdout redirect or a new seam -- not worth it. Preview-render
  ordering is already pinned by `plan_unlock_dry_run_render_2_closed_disks`.
- **Keyfile variant.** The dry-run gate is credential-source-agnostic (returns
  before `resolve_credential` regardless of passphrase vs keyfile), so the
  passphrase sentinel fully proves the gate ordering. A `--key-file` clone would
  be redundant for this invariant. (The keyfile *render* path is already covered
  by `plan_unlock_dry_run_render_2_closed_disks_with_key_file`.)
- **Production code.** No change; the gate already behaves correctly. This is
  pure regression-coverage.

## Verification

```
just test-rust
```

Expect the new test to pass. To confirm it actually pins the invariant
(catches the regression it targets), temporarily simulate the regression by
hoisting credential resolution above the dry-run gate in `cmd_unlock` --
insert before `if params.dry_run`:

```rust
let _credential = crate::credential::resolve_credential(
    params.passphrase_stdin,
    params.passphrase_file,
    params.key_file,
)
.map_err(MountError::from)?;
```

The `.map_err(MountError::from)?` is load-bearing: the error must **propagate**.
A `let _ = resolve_credential(...);` (result discarded) would swallow the
bogus-path failure, leave the test green, and prove nothing. (`MountError` is
already in scope in `unlock.rs`; `?` converts it to `UnlockError` via the
existing `#[from]`.) Re-run `just test-rust`: the new test must flip from
passing to failing. The sibling
`cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` will also fail
under this control -- the hoist runs before its empty-`to_unlock` check too --
and both firing confirms the credential-read guards work. Revert the temporary
edit afterward. No VM tests needed -- test-only, single-file, no production
behavior change.
