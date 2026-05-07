# Collapse the two close loops in `LockPlan::execute` via a helper

## Context

`LockPlan::execute` in `cli/src/lock.rs` has two loops that close LUKS
mappers: a membership loop (`cli/src/lock.rs:282-328`) that closes the
mappers backing pool members, and an orphan loop
(`cli/src/lock.rs:335-377`) that closes leftover `braid-*` mappers from
interrupted add/replace flows. Both loops carry the same
close-and-aggregate body: a `[wait] disk {name}: locking...` row, a
`close_mapper_with_retry` call, and a three-arm match
(`Ok` / `DeviceBusy if umount_error.is_some()` / `Err`) that updates the
shared `first_mapper_error` accumulator. The orphan loop adds tags
(`(orphan)`, `orphan close failed`, `orphan: `) on the four status
strings and prepends `name_from_mapper(entry).unwrap_or(entry)` whose
`unwrap_or` arm is unreachable -- `scan_orphan_mappers`
(`cli/src/lock.rs:37-55`) already filters non-`braid-` entries.

The duplication was introduced fresh in commit `4fcb6eb` (`fix(lock):
honor planned mapper close set`) when the close set was split into
`open_mappers` + `orphan_mappers`. Collapsing the bodies now keeps the
two paths in lockstep when retry behavior, error aggregation, or umount
suppression evolves -- a single edit point instead of two.

Outcome: ~40 fewer lines in `execute`, no behavior change, identical
stderr output, identical error-return semantics.

## Files to change

- `cli/src/lock.rs` -- add the helper, rewrite both close loops.

No other files change. Existing tests must pass unmodified; the
"Verification" section below distinguishes which strings are actually
byte-pinned (by VM `.py` tests) from which are preserved by code
review and which behaviors are gated by the Rust `mod tests`.

## Design

### New helper: `close_one_mapper`

A module-private free function in `cli/src/lock.rs`, sitting near
`umount_stderr_is_busy` (file's existing convention is free helpers --
`compile_lock_steps`, `scan_orphan_mappers`, `umount_stderr_is_busy`,
`orphan_scan_warn_body`, `orphan_mapper_warn_body`).

Per `AGENTS.md` "Doc Comments", the new top-level function gets a
short `///` comment. Suggested wording (3 lines):

```rust
/// Shared close-and-aggregate body for the membership and orphan
/// loops in `LockPlan::execute`, so status formatting, umount-busy
/// suppression, and `first_mapper_error` accumulation cannot drift.
```

Signature:

```rust
fn close_one_mapper<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
    disk_label: &str,
    is_orphan: bool,
    color_enabled: bool,
    umount_error: &Option<LockError>,
    first_mapper_error: &mut Option<LockError>,
) where
    R: CommandRunner,
    S: Sleeper,
```

Body responsibilities:

1. Emit `[wait] disk {disk_label}: locking{paren}...` where
   `paren = if is_orphan { " (orphan)" } else { "" }`.
2. Call `close_mapper_with_retry(runner, sleeper, mapper,
   color_enabled)`.
3. Match the three arms:
   - `Ok(())` -> emit `[ok] disk {disk_label}: locked{paren}`.
   - `Err(CloseMapperError::DeviceBusy(msg)) if umount_error.is_some()`
     -> emit `[warn] disk {disk_label}: {phrase} (umount was stuck):
     {msg}` where `phrase = if is_orphan { "orphan close failed" }
     else { "close failed" }`.
   - `Err(e)` -> emit `[fail] disk {disk_label}: {prefix}{err}` where
     `prefix = if is_orphan { "orphan: " } else { "" }`, then update
     `first_mapper_error` if `is_none()`.

The helper does NOT touch `all_already_closed` -- the caller flips it
unconditionally after invocation, mirroring current behavior. The
helper does NOT print "already closed" -- that lives in caller-side
short-circuits which differ between the two paths.

`color_enabled: bool` stays in the signature because
`close_mapper_with_retry` (`cli/src/mapper_close.rs:21-65`) takes it
for its own retry-warn (`mapper_close.rs:55-61`). Reconstructing the
inner `let line = |t, body| status_line(t, color_enabled, body)`
closure is a single line and matches the pattern at
`cli/src/lock.rs:176`.

### Membership loop rewrite (current `cli/src/lock.rs:282-328`)

```rust
for name in membership.disks.keys() {
    let mn = mapper_name(name);

    if !open_set.contains(mn.0.as_str()) {
        eprint!(
            "{}",
            line(StatusTag::Ok, &format!("disk {name}: already closed"))
        );
        continue;
    }

    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if !fs.exists(&mapper_path) {
        eprint!(
            "{}",
            line(StatusTag::Ok, &format!("disk {name}: already closed"))
        );
        continue;
    }

    close_one_mapper(
        runner,
        sleeper,
        &mn.0,
        name,
        false,
        color_enabled,
        &umount_error,
        &mut first_mapper_error,
    );
    all_already_closed = false;
}
```

The two pre-close short-circuits stay -- they have asymmetric output
("already closed" rows that the orphan path does not emit) and stay
local to the membership iteration over `membership.disks.keys()`.

### Orphan loop rewrite (current `cli/src/lock.rs:335-377`)

```rust
for entry in orphan_mappers {
    if !fs.exists(&format!("/dev/mapper/{entry}")) {
        continue;
    }
    // scan_orphan_mappers (lock.rs:44) only admits braid-* entries,
    // so name_from_mapper always returns Some here. Keep unwrap_or
    // as a graceful fallback if that invariant is ever bypassed.
    let disk_name = name_from_mapper(entry).unwrap_or(entry);
    close_one_mapper(
        runner,
        sleeper,
        entry,
        disk_name,
        true,
        color_enabled,
        &umount_error,
        &mut first_mapper_error,
    );
    all_already_closed = false;
}
```

Keep the `unwrap_or(entry)` rather than swap to `.expect(...)`:
`orphan_mappers` is `Vec<String>` with no type-level invariant, and
the `unwrap_or` is graceful degradation if anything ever bypasses the
scanner. Add the comment so the next reader does not re-flag it.

**Implementation note:** the current source at
`cli/src/lock.rs:340-341` uses `name_from_mapper(entry).expect(
"scan_orphan_mappers returns only braid-* mapper names")` (committed
in `ccee87e refactor(lock): document orphan-mapper invariant with
expect`). This refactor intentionally walks that back to
`unwrap_or(entry)` with the invariant comment -- a graceful fallback
is preferred over a panic on a soft-typed `Vec<String>` invariant,
and the orphan loop's `disk_label` only feeds the status rows
(degrading the row text is preferable to crashing the lock). The
revert is part of the same commit; do not leave both forms in the
tree.

### Reused functions and types

- `close_mapper_with_retry` and `CloseMapperError`
  (`cli/src/mapper_close.rs:21-65`) -- unchanged; helper just calls
  through.
- `status_line`, `StatusTag::{Wait, Ok, Warn, Fail}`,
  `color_enabled_for_stderr` (`cli/src/status_tag.rs`) -- unchanged.
- `LockError::from(CloseMapperError)`
  (`cli/src/lock.rs:22-30`) -- unchanged, called inside the helper's
  `Err` arm.
- `mapper_name` and `name_from_mapper`
  (`cli/src/config.rs:71-78`) -- unchanged.

## Verification

This is a pure refactor: zero behavior change, zero string change.
Test coverage splits into two distinct layers, and only some of the
helper's output is byte-pinned. The remainder must be preserved by
direct code review against the current source.

### Status rows actually byte-pinned by VM tests

These four literals are the only close-loop status strings any test
asserts on at the byte level. The refactor must preserve them
verbatim:

- `[wait] disk {name}: locking...` -- pinned by
  `tests/cli/braid-lock.py:78` (membership path).
- `[ok]   disk {name}: locked` -- pinned by
  `tests/cli/braid-lock.py:79` (membership path).
- `[wait] disk {name}: locking (orphan)...` -- pinned by
  `tests/cli/braid-lock-orphan.py:87` (orphan path).
- `[ok]   disk {name}: locked (orphan)` -- pinned by
  `tests/cli/braid-lock-orphan.py:88` (orphan path).

### Status rows preserved by code review only

No VM or Rust test asserts on these strings byte-for-byte. They are
preserved because this refactor is a textual transposition of the
existing format strings into the helper -- direct visual diff against
the current arms at `cli/src/lock.rs:282-377` is the gate:

- `[ok]   disk {name}: already closed` -- membership pre-close
  short-circuit (stays in the membership loop, not in the helper).
- `[warn] disk {name}: close failed (umount was stuck): {msg}` --
  membership busy-suppressed warn.
- `[fail] disk {name}: {err}` -- membership fatal fail.
- `[warn] disk {name}: orphan close failed (umount was stuck): {msg}`
  -- orphan busy-suppressed warn.
- `[fail] disk {name}: orphan: {err}` -- orphan fatal fail.

`tests/cli/braid-lock-umount-busy.py:53-54` only checks for the
`lsof` / `fuser` hint substring, not the busy-suppressed close warn.
`tests/cli/braid-lock-btrfs-held.py` only checks final mapper-closed
state, not the retry warn. Both end-to-end tests still gate the
behavior these strings narrate, just not the wording.

### Behavior coverage (Rust `mod tests`)

The Rust tests in `cli/src/lock.rs` assert call shape (`CmdRequest`
sequence) and `LockError` returns; they do not inspect stderr. They
gate that the refactor preserves:

- Pre-close short-circuit -- no cryptsetup close issued for
  already-closed members (`lock_already_locked`,
  `lock_partial_state`, `lock_happy_path_unmounts_and_closes`).
- Umount-suppressed busy does not escalate to fatal
  (`lock_umount_fails_busy_mapper_is_warning`,
  `lock_umount_fails_orphan_busy_is_warning`).
- Non-busy mapper errors stay fatal across umount failure
  (`lock_umount_fails_unexpected_mapper_error_is_fatal`,
  `lock_umount_fails_orphan_unexpected_error_is_fatal`).
- First-error-wins accumulation across both loops
  (`lock_collects_first_mapper_error`,
  `lock_orphan_close_failure_is_fatal`,
  `lock_continues_closing_after_mapper_error`).
- Retry path
  (`lock_retries_busy_close_then_succeeds`,
  `lock_busy_close_exhausts_retries_preserves_stderr_contract`).
- Lock returns `Ok(())` end-to-end with an orphan present
  (`lock_closes_orphaned_mapper`). Note this Rust test only asserts
  the success return; it does not inspect `MockRunner.requests()` to
  prove the orphan close call was issued -- unused mock outputs are
  silently ignored (`cmd.rs:980`). The actual gate that the orphan
  close happens is `tests/cli/braid-lock-orphan.py:111`, which
  asserts `/dev/mapper/braid-orphan` is gone after `braid lock`.

### Run

1. `just test-rust` -- behavior coverage above. Expected: all pass
   without test edits.
2. `just test-vm braid-lock braid-lock-orphan braid-lock-umount-busy
   braid-lock-btrfs-held` -- the four VM checks that exercise the
   close loops end-to-end. The first two are also where the four
   byte-pinned status rows above are asserted. Expected: pass.
3. `cargo build -p braid-cli` -- no new warnings.

### Code-review checklist (manual gate)

For each of the five non-byte-pinned status rows in the previous
section, diff the helper's `format!` against the corresponding format
string in the current `cli/src/lock.rs:282-377` arms and confirm the
template is character-for-character identical. This is the only gate
on the unpinned rows; treat it as required, not optional.

### Sanity check (post-implementation)

- `grep -n 'close_mapper_with_retry' cli/src/lock.rs` should show the
  call appears only inside `close_one_mapper`.
- `grep -n 'first_mapper_error = Some' cli/src/lock.rs` should show
  the assignment exists only inside `close_one_mapper`.
- `grep -n 'unwrap_or(entry)' cli/src/lock.rs` should show one hit in
  the orphan loop.
- `grep -n '\.expect(' cli/src/lock.rs` should show no
  `expect("scan_orphan_mappers returns only braid-*` -- confirming
  the current source's `.expect(...)` call was reverted in this
  refactor.
