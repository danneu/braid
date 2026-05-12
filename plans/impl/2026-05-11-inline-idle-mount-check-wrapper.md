# Inline Idle Mount Check Wrapper

## Summary

Simplify the idle mount probe by removing the private `idle::is_btrfs_mounted` pass-through helper. Keep the safety rationale on the shared public helper, but phrase it generically so `mount_check` does not mention idle-only result types.

## Key Changes

- In `cmd_idle`, replace the local helper call with:
  `crate::mount_check::is_btrfs_mounted(fs, mount_point.as_str())`
- Delete the private `is_btrfs_mounted` function from `idle.rs`.
- Remove the now-unused `MountInfoError` import from `idle.rs`.
- Update the doc comment on `mount_check::is_btrfs_mounted` to state:
  - it reads `/proc/self/mountinfo` through `Filesystem`
  - `Ok(false)` only means a well-formed mountinfo has no btrfs mount at the target
  - IO errors, malformed lines, and duplicate target entries are returned as `MountInfoError`
  - safety-critical callers should treat those errors as indeterminate/fail-closed

## Public API / Interface Changes

- No function signatures, types, or CLI behavior change.
- `mount_check::is_btrfs_mounted` remains the public shared helper; only its documentation becomes more complete.

## Test Plan

- Run `just test-rust`.
- No new tests are required because behavior is unchanged and existing coverage already checks:
  - idle maps mountinfo errors to `BusyReason::Unknown`
  - mount_check propagates mountinfo IO/parser errors
  - offline and mounted-btrfs cases still classify correctly

## Assumptions

- Do not move idle-specific wording like `BusyReason::Unknown`, `main.rs`, or autosuspend exit codes into `mount_check`; that mapping belongs in `idle.rs` and `docs/decisions/016-auto-suspend.md`.
- Do not add a replacement wrapper that accepts `&MountPoint`; direct `.as_str()` calls are already the established pattern for sibling mountinfo helpers.
