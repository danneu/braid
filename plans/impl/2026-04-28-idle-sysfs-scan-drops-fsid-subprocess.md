# Drop the two-subprocess fsid lookup in `braid idle`

## Context

`cmd_idle` in `cli/src/idle.rs` already reads the kernel exclusive-operation
state from `/sys/fs/btrfs/<fsid>/exclusive_operation` (commit `6423279`) and
reads `/proc/self/mountinfo` directly for the mount probe (commit `ac82986`).
Two subprocess calls remain in the non-scrub path purely to discover the
filesystem UUID:

1. `findmnt --json` (via `probe::probe_fsid`)
2. `btrfs filesystem show` (via `probe::probe_fsid`)

Neither produces information that is not already in sysfs. They exist only
because the sysfs read needs a UUID path component. This plan removes both
calls from `cmd_idle` by reading exclop state for every btrfs filesystem the
kernel exposes under `/sys/fs/btrfs/`, with "any busy = block suspend"
semantics. `probe_fsid` itself stays -- `cli/src/lock.rs` is a separate
caller and out of scope.

Outcome:

- `cmd_idle` issues zero subprocesses on the idle path beyond the
  irreplaceable `btrfs scrub status` probe.
- The sysfs read used by `cmd_idle` and the one used by mutating preflights
  (`preflight::check_no_exclusive_op`) come from the same helper file with
  the same parsing code, so they cannot disagree on what counts as busy.
- Multi-btrfs hosts (e.g. NixOS root on btrfs alongside the pool) are
  correct rather than silently broken: any in-flight exclop on any btrfs
  filesystem on the host blocks suspend. The `BusyReason` reported in that
  rare case may name an op on the non-pool fs, but the suspend decision is
  still correct -- autosuspend's whole job is to err conservative.

## Approach

Add a new sysfs scan helper next to the existing `check_no_exclusive_op`,
then point `cmd_idle` at it instead of `probe_fsid` + per-fsid sysfs read.

### New helper in `cli/src/preflight.rs`

Add immediately below `check_no_exclusive_op` (`preflight.rs:176-188`):

```rust
pub(crate) fn check_any_btrfs_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
) -> Result<(), ExclusiveOpError>
```

Body:

1. `entries = fs.list_dir("/sys/fs/btrfs")` -- propagate IO error as
   `ExclusiveOpError::Read`.
2. Skip known non-fsid entries by name before any read:
   `features` (always present) and `debug` (DEBUG-only kernels). Source:
   `reference/linux/fs/btrfs/sysfs.c:29-47` -- only `<uuid>/` dirs expose
   `exclusive_operation`.
3. For every other entry, attempt
   `fs.read_to_string("/sys/fs/btrfs/{entry}/exclusive_operation")`.
   - `Err(e)` -> `Err(ExclusiveOpError::Read(e))`. Any read error on a
     non-allowlisted entry, including `NotFound`, is fail-closed.
   - `Ok(s)` -> `ExclusiveOp::parse(s.trim())`:
     - `None` -> `Err(ExclusiveOpError::Unrecognized(s))`.
     - `Some(ExclusiveOp::None)` -> continue.
     - `Some(op)` -> immediate `Err(ExclusiveOpError::Busy(op))`.
4. After the loop:
   - If at least one fsid dir was successfully parsed as `none` -> `Ok(())`.
   - If zero fsid dirs were found after skipping the allowlisted pseudo-dirs
     -> `Err(ExclusiveOpError::Read(io::Error::new(NotFound, ...)))`.
     `is_btrfs_mounted` returned true earlier in `cmd_idle`; an empty
     `/sys/fs/btrfs/` after that is an invariant violation. Fail-closed.

Reuses the existing `ExclusiveOpError` vocabulary and the existing
`ExclusiveOp::parse` (`preflight.rs:80-94`), so `cmd_idle`'s existing match
arms (`idle.rs:103-109`) stay shape-compatible.

### `cli/src/idle.rs` changes

Replace lines 99-109 (the `probe_fsid` + `check_no_exclusive_op` block) with
a single call to `check_any_btrfs_exclusive_op(fs)`. Drop the
`crate::probe::{Filesystem, ProbeError, probe_fsid}` import down to just
`crate::probe::Filesystem`. Remove `IdleError::Probe` if no other call site
in this file produces `ProbeError`; otherwise leave it as a minor wart
(`IdleError` is `pub` and dropping a variant is a breaking change for
exhaustive matches in `main.rs` -- check before deleting).

The `BusyReason` enum, its `Display` impl, and `busy_from_exclop` (lines
17-50, 112-125) stay unchanged.

### `MockFs` extension in `cli/src/idle.rs::tests`

Today's `MockFs` (lines 171-261) tracks a single `expected_path`. The new
helper iterates `/sys/fs/btrfs/` and reads multiple sysfs paths, so migrate
to a `HashMap<String, Result<String, std::io::ErrorKind>>` keyed by full
sysfs path, plus a seeded `Vec<String>` for `list_dir("/sys/fs/btrfs")`.
Preserve the strict "unexpected path = NotFound" behavior at lines 251-254
-- it has caught regressions and is the whole point of routing through this
mock.

Suggested constructor shapes (keep existing call sites one-liners):

- `with_exclop(body)` -- seeds `list_dir("/sys/fs/btrfs") -> [FSID]` and
  `/sys/fs/btrfs/{FSID}/exclusive_operation -> body`. Used by every existing
  test that called `with_exclop`.
- `with_read_error()` -- seeds `list_dir -> [FSID]` and the per-fsid read
  returning `PermissionDenied`. This keeps the existing read-error test
  focused on non-`NotFound` IO errors; `NotFound` on a real listed entry is
  covered separately by `idle_unknown_entry_notfound_is_fail_closed`.
- `with_offline_mountinfo()` / `with_no_mountinfo()` / `with_mountinfo()` --
  unchanged shape; still seed mountinfo and never reach the sysfs branch.

### Test updates in `cli/src/idle.rs::tests`

Delete:

- `seed_fsid_probe` helper (lines 347-353).
- `btrfs_show` helper (lines 285-300).
- `findmnt_json` / `findmnt_mounted` helpers (lines 263-283) -- nothing in
  `cmd_idle` calls findmnt anymore.

Simplify (drop `seed_fsid_probe`, drop the runner mocks, keep the MockFs
seeding):

- `idle_when_all_ops_quiet` (line 370)
- `ready_for_sysfs_check` (line 410) and its eight callers
  `busy_when_balance` through `busy_when_swap_activate` (lines 430-476) and
  `error_on_unrecognized_exclop` (line 484)
- `error_on_sysfs_read_failure` (line 495)
- `no_balance_or_replace_subprocess_calls` (line 519)

Unchanged:

- `idle_when_pool_offline` (line 359)
- `busy_when_scrub_running` (line 390)
- `error_on_scrub_probe_failure` (line 532)
- `mountinfo_read_failure_is_not_pool_offline` (line 554)
- `mountinfo_malformed_target_line_is_not_pool_offline` (line 574)

New tests (each with the standard `Intent / Why / Scenario` block per
`AGENTS.md` "Test Conventions"):

1. **`idle_skips_features_and_debug_pseudo_dirs`** -- seed
   `list_dir("/sys/fs/btrfs") -> ["features", "debug", FSID]`, do not seed
   reads for the two pseudo-dirs, and return `"none"` for FSID's. Expect
   `IdleResult::Idle`. Pins the kernel-pseudo-dir allowlist from
   `reference/linux/fs/btrfs/sysfs.c:29-47`.
2. **`idle_unknown_entry_notfound_is_fail_closed`** -- seed two non-allowlisted
   entries, return `"none"` for one, and leave the other unseeded so its
   `exclusive_operation` read returns `NotFound`. Expect `IdleError::Exclop`.
   Pins that `NotFound` on a real listed entry is fail-closed rather than
   mistaken for a pseudo-dir.
3. **`idle_any_busy_blocks_suspend_multi_btrfs`** -- seed two fsid dirs,
   one `none` and one `balance`. Expect `IdleResult::Busy(Balance)`.
   Documents the any-busy semantic and prevents a future "scope to pool
   fsid" change from silently passing.
4. **`idle_zero_fsid_dirs_after_mount_check_is_error`** -- mountinfo seeded
   as mounted, `list_dir("/sys/fs/btrfs") -> []`. Expect `IdleError::Exclop`.
   Pins the invariant-violation fail-closed branch.
5. **`idle_list_dir_io_error_is_fail_closed`** -- mountinfo seeded as
   mounted, scrub seeded as completed, `list_dir("/sys/fs/btrfs")` returns
   `PermissionDenied` (not NotFound -- that's the empty-listing branch
   covered by RealFilesystem at probe.rs:47). Expect `IdleError::Exclop`.
   Pins that the helper propagates `list_dir` IO errors as
   `ExclusiveOpError::Read` -- without this test a future change could
   silently treat `PermissionDenied` / `EIO` on `/sys/fs/btrfs` as idle.
   Requires extending `MockFs` with a constructor that seeds an error for
   `list_dir("/sys/fs/btrfs")` (the current `list_dir` impl at line 258
   unconditionally returns `Ok(vec![])`, so this is a real new mock seam).

### Critical files

- `cli/src/preflight.rs` -- add `check_any_btrfs_exclusive_op` below
  `check_no_exclusive_op` (line 188).
- `cli/src/idle.rs` -- swap the call site (lines 99-109) and update
  `MockFs` + tests.
- `docs/decisions/016-auto-suspend.md` -- replace the stale note at line 51
  ("`cmd_idle` continues to call `probe_fsid` after the mount check, and
  `probe_fsid` still uses `findmnt` internally...") with a paragraph that:
  (a) describes the `/sys/fs/btrfs/*` scan as the post-mount-check exclop
  source for `cmd_idle`,
  (b) states the any-busy semantic explicitly and the multi-btrfs caveat,
  (c) notes that `probe_fsid` remains for non-idle callers (`lock.rs` and
  preflight pipelines) and is no longer reached from `cmd_idle`.
  Same section also referenced from `idle.rs:136`; that comment can stay
  pointing at the doc.

### Out of scope

- `cli/src/probe.rs::probe_fsid` stays. `cli/src/lock.rs` and the
  preflight-status pipelines still rely on it. Tests at `probe.rs:1455`,
  `1483`, `1508` stay.
- Parser fixtures: no new parsers, no fixture refresh needed.

## Verification

1. **Unit tests:** `just test-rust` -- the new helper has direct coverage
   via the five new idle tests plus the simplified existing tests. The
   `parse_btrfs_filesystem_show` and `parse_findmnt_json` parsers still
   have other callers, so their tests stay green without changes.
2. **VM tests:** `just test-vm` -- the existing
   `tests/cli/replace-inhibits-suspend.py` (or whatever the current
   sysfs-derived `device replace` busy-reason test is named after the
   `6423279` rename) exercises the end-to-end path inside a real NixOS VM
   with a real `/sys/fs/btrfs/`. A passing run proves the sysfs scan finds
   the fsid dir and reports `device replace`. Spot-check by running just
   that file: `just test-vm replace-inhibits-suspend` (verify the actual
   filename via `ls tests/cli/`).
3. **Manual sanity in the VM:** boot the autosuspend test VM, run
   `braid idle` against an unlocked pool with no exclop -> expect exit 0
   "idle"; trigger a `btrfs balance start` and re-run -> expect exit 1
   "balance running" sourced from the sysfs scan.
4. **No subprocess regression:** the existing `no_balance_or_replace_subprocess_calls`
   test (idle.rs:519) already asserts that `BtrfsBalanceStatus` and
   `BtrfsReplaceStatus` are not invoked. After this change, that test also
   covers the absence of `BtrfsFilesystemShow` and `FindmntJson` calls
   from `cmd_idle` (they have no mocks and a `MissingMock` would fail the
   test).
