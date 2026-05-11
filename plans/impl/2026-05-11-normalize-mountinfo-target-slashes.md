# Plan: normalize trailing slashes in mountinfo target comparison

## Context

`cli/src/mount_check.rs:59` compares a configured mount-point string
against the canonical mount-point field emitted by the kernel in
`/proc/self/mountinfo` with `parsed.mount_point == target`. The kernel
never emits a trailing slash for non-root mounts (`reference/linux/fs/proc_namespace.c:135-191`, `seq_path_root`), so any caller
that passes a target with a trailing slash silently misses the entry and
the helper returns `Ok(None)`.

The safety-critical seam is `cmd_idle` (`cli/src/idle.rs:62-68`):
`Ok(None)` -> `Ok(false)` -> `IdleResult::PoolOffline` -> exit 0 ->
`bash -c '! braid idle'` returns 1 -> autosuspend allows suspend on a
mounted pool. This is precisely the fail-open seam the file-based
mountinfo probe exists to prevent (`docs/decisions/016-auto-suspend.md`,
"Mount probe reads `/proc/self/mountinfo` directly").

Three more call sites share the same exact-string match:

- `cli/src/probe.rs:217` (`probe_pool`) -- treats trailing-slash configs
  as "pool not mounted"; downstream `cmd_status` and friends report an
  offline pool.
- `cli/src/probe.rs:346` (`probe_fsid`) -- errors with "mount point not
  present in mountinfo"; mutating commands needing the fsid abort.
- `cli/src/preflight.rs:263` (`check_not_read_only`) -- maps Ok(None)
  to a `PreviewNote::Warn` ("ro guard did not run"); the read-only
  refusal is silently downgraded to a warning.

The finding's reassurance that `lib.types.path` normalizes NixOS-driven
configs is misleading: `lib.types.path`'s `check` only verifies the
value is string-like and starts with `/`; it does not strip trailing
slashes from strings. So even a NixOS configuration with
`braid.mountPoint = "/mnt/storage/"` flows through to
`/etc/braid/config.json` unchanged. Hand-edits to the JSON and any
future config source that feeds `config_read` -> `MountPoint(...)` ->
the four mount_check call sites are exposed.

(The `--mount`-argument scrub commands in `cli/src/main.rs:611,622,638`
are NOT in scope: `cli/src/scrub_cancel.rs`,
`cli/src/scrub_needs_resume.rs`, and `cli/src/scrub_resume_or_start.rs`
do not call into `mount_check` -- they pass the mount path straight to
btrfs subprocesses, which tolerate trailing slashes via kernel path
resolution.)

Goal: normalize the target string once, at the single chokepoint where
the four affected call sites converge (idle, probe_pool, probe_fsid,
check_not_read_only).

## Approach

Add normalization at the entry of
`mount_check::find_unique_target_entry`. Both
`fstype_at_mount`/`mount_entry_at` (and their `*_via_fs` wrappers, and
`is_btrfs_mounted`) flow through this helper, so one change covers all
four production callers without touching `MountPoint`, `Config`, or any
construction site.

Normalization rule: strip one or more trailing `/` from the target,
preserving target `"/"` (the root mount) verbatim. `trim_end_matches('/')`
with a `target == "/"` guard. The parsed mount_point from mountinfo is
NOT normalized -- it is already canonical by construction, and leaving
the parser strict on its kernel-emitted input preserves the existing
"strict on every line" policy that the file's tests pin.

Rejected: a `MountPoint::new()` constructor with field privatization.
Trade-off: stronger invariant (canonical-by-construction) but ~150
`MountPoint(...)` tuple construction sites would need updating, and
the change breaks the codebase-wide raw-newtype pattern shared by
`MapperName`, `LuksUuid`, `ByIdPath`, and `MountPoint`. A later
project-wide canonical-by-construction refactor across all four newtype
types could revisit this; it is not justified by this finding alone.

## Files to modify

- `cli/src/mount_check.rs` -- one helper change + three new tests in the
  existing `#[cfg(test)] mod tests` block.

No other production source touched. No tests in other files modified.

## Implementation

1. In `cli/src/mount_check.rs`, modify `find_unique_target_entry`
   (currently lines 47-69) to normalize the target before the
   comparison loop:

   ```rust
   fn find_unique_target_entry(
       content: &str,
       target: &str,
   ) -> Result<Option<ParsedLine>, MountInfoError> {
       // mountinfo emits canonical paths (no trailing slash for non-root
       // mounts; see reference/linux/fs/proc_namespace.c:135-191). Strip
       // trailing slashes from the target so a configured path like
       // "/mnt/storage/" still matches the kernel's "/mnt/storage". Root
       // ("/") is preserved verbatim because the kernel emits "/" for it.
       // This closes the fail-open seam the mountinfo probe exists to
       // prevent (docs/decisions/016-auto-suspend.md): a trailing-slash
       // config would otherwise make is_btrfs_mounted return Ok(false)
       // and let cmd_idle conclude PoolOffline on a mounted pool.
       let target = if target == "/" {
           target
       } else {
           target.trim_end_matches('/')
       };
       let mut hit: Option<ParsedLine> = None;
       for line in content.lines() {
           ...
       }
       Ok(hit)
   }
   ```

   The parsed `mount_point` field stays unmodified -- only the
   user-supplied target is canonicalized.

2. The existing `is_btrfs_mounted`, `fstype_at_mount`,
   `fstype_at_mount_via_fs`, `mount_entry_at`, and `mount_entry_at_via_fs`
   helpers all route through `find_unique_target_entry`, so no further
   code changes are required.

## Tests

Add three new tests to the existing `#[cfg(test)] mod tests` block in
`cli/src/mount_check.rs`. Follow the project's
intent/why/scenario preamble convention (see existing tests in the same
file).

1. `fstype_at_mount_matches_trailing_slash_target` -- seed canonical
   mountinfo body with `/mnt/storage` mounted as btrfs and query with
   `"/mnt/storage/"`. Assert `Ok(Some("btrfs".into()))`. Pins the parser
   behavior at the helper all four call sites share. Preamble must name
   the autosuspend fail-open seam as the scenario.

2. `mount_entry_at_matches_trailing_slash_target` -- structurally pairs
   with (1). Same canonical `/mnt/storage` mountinfo body with full
   vfs_options / fs_options, queried via `mount_entry_at` with
   `"/mnt/storage/"`. Assert the full `MountEntry { fstype,
   vfs_options, fs_options }`. This is the test that pins the
   read-only preflight path (`cli/src/preflight.rs:263`,
   `check_not_read_only` -> `mount_entry_at_via_fs`) and stays valid
   even if a future refactor stops routing `mount_entry_at` through
   `find_unique_target_entry`. Without this test the coverage is
   structure-sensitive: a refactor that duplicates the target match
   into `mount_entry_at` could silently drop normalization there
   while (1) still passes.

3. `fstype_at_mount_root_target_still_matches_root_entry` -- regression
   guard that the `target == "/"` branch preserves the root case.
   Body contains the existing `ROOT_LINE` constant and queries with
   `"/"`; assert `Ok(Some("ext4".into()))` (the constant mounts ext4
   at `/`). Pins the guard against a future change that drops the `/`
   special case.

## Out of scope

- Double leading slashes, embedded `//`, leading whitespace, and other
  general path canonicalization. The finding focuses on trailing
  slashes; broader normalization is a separate, larger change and not
  justified by any observed failure mode.
- `MountPoint::new()` constructor / canonical-by-construction newtype
  refactor (see Rejected in Approach above).
- Other newtype wrappers (`MapperName`, `LuksUuid`, `ByIdPath`) that
  share the raw-newtype pattern. They do not flow through mountinfo
  comparison, so the same bug class does not apply.

## Verification

- `just test-rust` -- runs the CLI's Rust unit tests, including the new
  cases in `cli/src/mount_check.rs`.
- The existing test matrix already exercises every other call path
  through `find_unique_target_entry` (octal escapes, optional fields,
  multi-line bodies, malformed lines, duplicates, UTF-8 preservation,
  trailing junk, empty source fields, IO-shimmed wrappers). The three
  new tests are additive; no existing tests need to change.
- No VM test changes are required. The fix is purely parser-layer
  behavior; VM-level idle/probe/preflight behavior is already covered
  by the existing test suite once the unit-level invariant is pinned.

## Critical files referenced

- `cli/src/mount_check.rs` (the file being modified)
- `cli/src/idle.rs:62-68` (safety-critical caller)
- `cli/src/probe.rs:217,346` (sibling callers)
- `cli/src/preflight.rs:263` (sibling caller, read-only guard)
- `docs/decisions/016-auto-suspend.md` (fail-closed contract the fix
  preserves)
- `reference/linux/fs/proc_namespace.c:135-191` (kernel's canonical
  mount_point emission, justifying why only target normalization is
  needed)
