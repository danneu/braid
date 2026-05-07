# Dedupe lock close-set path construction

## Context

`cli/src/lock.rs` builds the `[/dev/mapper/X, /dev/mapper/Y]` close-set
chain in two near-identical places:

- `compile_lock_steps` at `cli/src/lock.rs:99-103` (for the dry-run
  preview's `BtrfsDeviceScanForget` step; unfiltered).
- `LockPlan::execute` at `cli/src/lock.rs:238-244` (for the real
  forget call; appends `.filter(|p| fs.exists(p))` as a TOCTOU guard
  on the disappearance window between plan and execute).

Both chain `open_mappers.iter().chain(orphan_mappers.iter()).map(|m|
format!("/dev/mapper/{m}"))`. The `lock_forget_devices` helper that
previously deduplicated this was deleted in commit `4fcb6eb` (2026-05-06,
"fix(lock): honor planned mapper close set") when its body got inlined
into `execute`; the inline copy still mirrors the construction in
`compile_lock_steps`.

The original review finding pitched storing a precomputed `forget_devs:
Vec<String>` on `LockPlan`. That works, but it adds derived state to
the struct that has to stay in sync with the existing `open_mappers` +
`orphan_mappers` fields. A small private free helper eliminates the
duplication with less surface area while keeping the TOCTOU re-filter
visible at the call site where it matters.

## Approach

Add a private free helper next to the other small helpers near the top
of `cli/src/lock.rs` (alongside `orphan_scan_warn_body`,
`orphan_mapper_warn_body`, `umount_stderr_is_busy`):

```rust
/// Compose the lock close set as fully qualified `/dev/mapper/...`
/// paths: every membership mapper observed open at plan time,
/// followed by orphaned braid-* mappers, in that order. Caller is
/// responsible for any TOCTOU re-filter -- this helper does not
/// touch the filesystem.
fn close_set_paths(open_mappers: &[String], orphan_mappers: &[String]) -> Vec<String> {
    open_mappers
        .iter()
        .chain(orphan_mappers.iter())
        .map(|m| format!("/dev/mapper/{m}"))
        .collect()
}
```

Then collapse the two call sites:

- `compile_lock_steps` (`cli/src/lock.rs:99-103`) becomes
  `let forget_devs = close_set_paths(open_mappers, orphan_mappers);`.
- `LockPlan::execute` (`cli/src/lock.rs:238-244`) becomes
  ```rust
  let mut forget_devs = close_set_paths(&self.open_mappers, orphan_mappers);
  forget_devs.retain(|p| fs.exists(p));
  ```

Surrounding logic (the `if !forget_devs.is_empty()` guards and the
`BtrfsDeviceScanForget` request) stays put.

The order of paths the helper produces is byte-for-byte the same as
both existing chains, so no test fixtures or assertions need to change.
No changes to `LockPlan`'s shape, no new fields, and the per-mapper
close loops below the forget step are unaffected.

## Critical files

- `cli/src/lock.rs` -- the only file modified.

## Existing patterns reused

- Small private free helper style: `orphan_scan_warn_body`
  (`cli/src/lock.rs:60-62`), `orphan_mapper_warn_body`
  (`cli/src/lock.rs:67-69`), `umount_stderr_is_busy`
  (`cli/src/lock.rs:74-80`). Same shape, same place in the file.
- Plan-time `fs.exists` filtering already lives at
  `cli/src/lock.rs:431` (membership mappers) and
  `cli/src/lock.rs:50` (orphan scan); the helper deliberately does not
  re-do that work.

## Out of scope

- `cli/src/recover.rs:333-336` builds a similar `forget_devs` chain
  during the recover remount cycle, but its input is disk names piped
  through `config::mapper_name(...)` rather than already-computed
  mapper-name strings. Different shape, not worth contorting one helper
  to serve both. Left untouched.

## Verification

- `just test-rust` -- runs the `lock.rs` unit tests, including the
  forget-set assertions:
  - `lock_adds_forget_after_umount` (`cli/src/lock.rs:1058-1081`)
  - `lock_forget_failure_is_nonfatal` (`cli/src/lock.rs:1083-1124`)
  - `lock_closes_orphaned_mapper` (`cli/src/lock.rs:1133-1177`)
  - `execute_does_not_close_membership_mapper_absent_from_plan`
    (`cli/src/lock.rs:766-809`)
  - `lock_orphan_scan_failure_is_nonfatal`
    (`cli/src/lock.rs:1185-1240`)
- `just test-vm` (filtered to the lock-related VM tests if known) --
  exercises `cmd_lock` against real `cryptsetup` + `btrfs` in a NixOS
  VM to confirm the forget-and-close path still works end-to-end.
- `cargo fmt` + `cargo clippy` -- format and lint check after the edit.
