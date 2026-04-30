# Plan: Announce wait in unlock and shared mount helpers

## Context

The recent verify-against-every-relevant-disk change in `braid unlock`
made silent waits in the unlock pipeline visible. On a 3-disk pool:

```
[ok]   disk toshiba-pro-00ff: found
[ok]   disk toshiba-pro-02af: found
[ok]   disk toshiba-z5g2: found
LUKS passphrase:
[wait] passphrase: checking against toshiba-pro-00ff...
[wait] passphrase: checking against toshiba-pro-02af...
[wait] passphrase: checking against toshiba-z5g2...
[ok]   disk toshiba-pro-00ff: unlocked       <-- silent gap (Argon2 again)
[ok]   disk toshiba-pro-02af: unlocked
[ok]   disk toshiba-z5g2: unlocked
[ok]   pool: mounted /mnt/storage             <-- silent gap (scan+mount)
```

cryptsetup re-derives Argon2 inside `luksOpen` even after `--test-passphrase`
already verified, so each per-disk unlock is its own slow window. The mount
phase wraps `btrfs device scan` + `mkdir` + `mount`, any of which can stall.
None of those windows are announced.

This plan closes the unlock gaps and pins the new output with VM
assertions. The shared mount helpers also serve `braid recover`, so the
output change ships there too.

A canonical principle (`docs/principles.md`) was considered but **not**
added in this round: it would assert a project-wide rule that `add`,
`replace`, `remove`, `remove-missing`, `recover` mid-mutation,
`enroll`, and `lock` are all expected to satisfy, but those commands
keep their existing silent gaps until follow-up work brings them into
compliance. Promoting this to a principle is a follow-up step, not
this one.

## Scope (this round)

1. New ADR `docs/decisions/021-wait-in-unlock.md` whose decision is
   narrowly scoped: unlock + the shared mount helpers
   (`execute_mount_only`, `execute_unlock_and_mount`, `scan_and_mount`).
   Other commands are explicitly noted as not yet bound.
2. Three `[wait]` insertions in `cli/src/mount.rs`:
   - Before each per-disk `cryptsetup luksOpen` (passphrase arm).
   - Before each per-disk `cryptsetup luksOpen` (keyfile arm).
   - Before `BtrfsDeviceScanAll` in `scan_and_mount`, covering scan +
     mkdir + mount as a single mount phase.
3. VM-test assertions pinning the new lines so the change cannot silently
   regress.
4. `docs/index.md` index entry for the new ADR.

## Out of scope (follow-up list at the bottom)

- Parallelizing verify/unlock (already deferred).
- A canonical `docs/principles.md` principle for "announce every wait".
  Defer until the other commands listed at the bottom comply.
- Applying the rule to `add`, `replace`, `remove`, `remove-missing`,
  `recover`'s own (non-shared) slow paths, `enroll`, `lock`.
- Adding terminal `[ok]` rows after each `[wait] passphrase: checking ...`
  line. Today the next `[wait]` (or first per-disk `[ok]`) is the
  implicit acknowledgement; revisit if it feels off in practice.

## Affected commands (callers of the changed helpers)

- **`braid unlock`** (`cli/src/unlock.rs`) -- primary target. Routes
  through `execute_mount_only` (when every mapper is already open) or
  `execute_unlock_and_mount` (the normal path).
- **`braid recover`** (`cli/src/recover.rs`) -- collateral. The recover
  path that mounts the pool from the post-recovery state calls the same
  shared helpers, so `[wait] disk X: unlocking...` and `[wait] pool:
  mounting ...` will appear there too. This is desired.

`braid add`, `braid replace`, `braid remove`, etc. do **not** route
through these helpers and are unaffected.

## Files to modify

### 1. `cli/src/mount.rs` -- three insertions

All three reuse the helper already imported at `mount.rs:10`:
`use crate::status_tag::{StatusTag, color_enabled_for_stderr, status_line};`

**Insertion A** -- per-disk passphrase unlock. Inside
`open_disks_with_passphrase` at `mount.rs:547-587`, just inside the
`for` loop, before the `luks::ensure_luks_open(...)` call:

```rust
for (name, by_id) in to_unlock {
    eprint!(
        "{}",
        status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("disk {name}: unlocking..."),
        )
    );
    if let Err(e) = luks::ensure_luks_open(runner, fs, name, by_id, passphrase) {
        // ... existing error handling unchanged
    }
    eprint!(
        "{}",
        status_line(
            StatusTag::Ok,
            color_enabled,
            &format!("disk {name}: unlocked"),
        )
    );
}
```

**Insertion B** -- per-disk keyfile unlock. Mirror of A inside
`open_disks_with_key_file` at `mount.rs:638-680`, just inside the
`for` loop, before the `luks::ensure_luks_open_with_key_file(...)`
call. Same wording (`"disk {name}: unlocking..."`).

**Insertion C** -- pool mount phase header. Inside `scan_and_mount`
at `mount.rs:758-809`, the `[wait]` must go **above** the
`BtrfsDeviceScanAll` call on line 767, so the line covers scan +
mkdir + mount as a single mount phase:

```rust
fn scan_and_mount<...>(...) -> Result<bool, MountError> {
    let mount_point = config.mount_point();

    eprint!(
        "{}",
        status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("pool: mounting {mount_point}..."),
        )
    );

    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    // ... rest unchanged
}
```

The terminal `[ok] pool: mounted {mount_point}` line at
`mount.rs:799-806` is unchanged.

### 2. `docs/decisions/021-wait-in-unlock.md` (new file)

Use YAML frontmatter (per `docs/index.md:3-13`) and the standard ADR
sections seen in `020-ups-integration.md`. Skeleton:

```markdown
---
intent: Capture why `braid unlock` and the shared mount helpers
  announce every blocking step with a `[wait]` row, why the rule is
  not yet a project-wide principle, and which commands are bound by
  this ADR today. Read before changing unlock UX or the mount
  helpers.
---

# Wait rows in unlock and shared mount helpers

Status: Active

> Principles:
> - (none yet -- promote to a principle once the rest of the
>   interactive commands comply)

## Context

(Describe the verify-everywhere unlock change, the silent gaps it
exposed in per-disk luksOpen and the mount phase, and that
cryptsetup re-derives Argon2 inside luksOpen even after
`--test-passphrase`.)

## Options considered

1. TTY spinner / progress bar -- rejected: needs a TTY, fights log
   capture, awkward in `braid-auto-unlock.service` journals.
2. Best-effort ad-hoc waits as gaps surface -- rejected: gaps recur.
3. Codify "[wait] before every blocking step" as a project principle
   *now* -- rejected: would immediately contradict `add`, `replace`,
   `remove`, `remove-missing`, `recover`'s own slow paths, `enroll`,
   `lock`. Principles are authoritative
   (`docs/principles.md:3`); a principle the codebase doesn't satisfy
   on the day it lands is a documentation bug.
4. Scope the rule to `braid unlock` and the shared mount helpers
   today; promote to a principle once the other commands comply.
   **Accepted**.

## Decision

- `braid unlock` (and `braid recover`'s mount tail, which shares the
  helpers) emit a `[wait]` line before any blocking step:
  - per-disk `cryptsetup luksOpen` (passphrase and keyfile arms),
  - the mount phase (`btrfs device scan` + `mkdir` + `mount`).
- The `[wait]` line uses
  `status_tag::status_line(StatusTag::Wait, ...)` and is closed by
  the existing per-step success line.
- Other interactive commands keep their current behavior until they
  are individually updated.

## Tradeoffs accepted

- Slightly more verbose stderr.
- Enforcement for the in-scope helpers is by VM-test assertion;
  enforcement project-wide is deferred until promotion to a
  principle.
- `braid recover` inherits the new rows automatically because the
  helpers are shared. This is desirable.
```

### 3. `docs/index.md` -- add ADR 021 to the decisions list

Insert after the `020-ups-integration.md` line at `docs/index.md:51`:

```markdown
- [decisions/021-wait-in-unlock.md](decisions/021-wait-in-unlock.md) -- **Active.** `braid unlock` (and `braid recover`'s shared mount tail) emit a `[wait]` row before per-disk LUKS open and before the mount phase; project-wide promotion to a principle deferred until other interactive commands comply.
```

The `principles.md` summary line at `docs/index.md:17` ("Twelve canonical
invariants ...") does **not** change in this round, since no principle
is added.

### 4. VM-test assertions -- pin the new lines

The Rust unit tests in `cli/src/unlock.rs` and `cli/src/mount.rs` do not
capture stderr, so they keep passing without change. The pinning lives
in the existing VM tests, which already grep stderr (see
`tests/cli/braid-unlock.py:79-85`).

**`tests/cli/braid-unlock.py`** -- extend the existing block that
asserts `[wait] passphrase: checking against disk1...` precedes the
first `[ok] disk disk1: unlocked` (around line 79). Add:

```python
unlocking_wait = "[wait] disk disk1: unlocking...\n"
mounting_wait  = "[wait] pool: mounting /mnt/storage...\n"

assert unlocking_wait in probe_err, (
    f"per-disk unlocking wait row missing, got: {probe_err!r}"
)
assert probe_err.find(unlocking_wait) < probe_err.find(unlocked_line), (
    f"unlocking wait must precede unlocked row, got: {probe_err!r}"
)
assert mounting_wait in probe_err, (
    f"pool mounting wait row missing, got: {probe_err!r}"
)
mounted_line = "[ok]   pool: mounted /mnt/storage\n"
assert probe_err.find(mounting_wait) < probe_err.find(mounted_line), (
    f"mounting wait must precede mounted row, got: {probe_err!r}"
)
```

**`tests/cli/braid-unlock-key-file.py`** -- mirror the per-disk
unlocking-wait check next to the existing keyfile assertions at
lines 66-72. Pool-mounting is also covered here (same shared
helper), so add the mounting wait + ordering check too.

**`tests/cli/braid-recover.py`** -- this file already asserts
`[wait] passphrase: checking against disk1...` precedes
`[ok] disk disk1: unlocked` around lines 273-279. Extend with the
same `unlocking_wait` and `mounting_wait` checks so a regression
in the shared helpers is caught from the recover side too.

These are line-count and substring assertions only -- they will keep
passing even if other lines are added between them, so they are
robust to incidental future changes.

## Output after the change

```
[ok]   disk toshiba-pro-00ff: found
[ok]   disk toshiba-pro-02af: found
[ok]   disk toshiba-z5g2: found
LUKS passphrase:
[wait] passphrase: checking against toshiba-pro-00ff...
[wait] passphrase: checking against toshiba-pro-02af...
[wait] passphrase: checking against toshiba-z5g2...
[wait] disk toshiba-pro-00ff: unlocking...
[ok]   disk toshiba-pro-00ff: unlocked
[wait] disk toshiba-pro-02af: unlocking...
[ok]   disk toshiba-pro-02af: unlocked
[wait] disk toshiba-z5g2: unlocking...
[ok]   disk toshiba-z5g2: unlocked
[wait] pool: mounting /mnt/storage...
[ok]   pool: mounted /mnt/storage
```

## Verification

1. `just test-rust` -- existing Rust unit tests in `cli/src/unlock.rs`
   and `cli/src/mount.rs` do not assert on stderr text, so no Rust test
   updates are required. They should keep passing unchanged.

2. `just test-vm braid-unlock braid-unlock-key-file braid-recover` --
   the three updated VM tests must pass against `nixos-25.11`. These
   assertions are the regression net for the new lines.

3. Manual / VM end-to-end: bring up a 3-disk test VM (e.g. via the
   standard unlock VM checks), run `sudo braid unlock`, and confirm
   the output shape matches the snippet above. Then:
   - Run `braid unlock` with the pool **already mounted**:
     `cmd_unlock` returns early at `unlock.rs:93-96` because
     `plan.open_plan` is `None`, so neither helper runs and **no new
     wait rows** appear -- only the existing `pool already mounted at
     ...` info note.
   - Run `braid unlock` with the pool **unmounted but every mapper
     already open** (mount-only branch via `execute_mount_only`):
     only `[wait] pool: mounting /mnt/storage...` should appear,
     because `to_unlock` is empty so no per-disk unlock waits run.
   - Run with `--key-file`: confirm the keyfile arm also emits
     per-disk `[wait]` lines.
   - Run `braid recover` after a simulated mid-mutation crash and
     confirm the same per-disk and mount waits appear during the
     post-recover mount tail.

4. Output formatting sanity: confirm with `NO_COLOR=1 sudo braid unlock`
   that the new lines align at column 7. The existing
   `status_line_prefix_is_seven_visible_columns` test in
   `cli/src/status_tag.rs:132` already pins this for `StatusTag::Wait`.

## Follow-up: other interactive sites that will need `[wait]`

These are the gaps surfaced by the codebase scan, ordered roughly by
user-visible severity. Each is a candidate for a follow-up plan.
**Once all of these comply**, promote the ADR to a numbered
principle in `docs/principles.md` and update `docs/index.md`'s
"Twelve canonical invariants" line.

- **`braid add`** (`cli/src/add.rs`)
  - `luks_format()` (cryptsetup luksFormat, ~10s+ per disk for Argon2);
    currently emits only `eprintln!("LUKS formatted:")` after the fact
    near `add.rs:585`.
  - `pool_balance_raid1()` (btrfs balance to RAID1, potentially hours);
    currently emits only `eprintln!("Balancing to RAID1...")` near
    `add.rs:679`. The existing progress module output is fine for live
    updates but a leading `[wait]` is missing.

- **`braid replace`** (`cli/src/replace.rs`)
  - `luks_format()` for the new disk (`replace.rs:302`).
  - `btrfs replace start` itself (`replace.rs:352`) -- can run hours.
  - Soft balance after replacing a missing device (`replace.rs:377+`).

- **`braid remove`** (`cli/src/remove.rs`)
  - Pre-remove RAID1 -> single balance (when shrinking from 2 to 1).
  - `btrfs device remove` itself.

- **`braid remove-missing`** (`cli/src/remove_missing.rs`)
  - Optional soft balance after clearing the last missing device.

- **`braid recover`** (`cli/src/recover.rs`) -- the parts NOT covered
  by this plan's shared-helper change:
  - Resume paused balance (`recover.rs:652`).
  - Replay RAID1 soft balance (`recover.rs:662`).

- **`braid lock`** (`cli/src/lock.rs`)
  - `umount` (can briefly block on fd closure / inhibitors).
  - `cryptsetup close` retry loop -- emits `[warn]` on busy retry but
    no leading `[wait]` for the close attempt itself.

- **`braid enroll`** (`cli/src/enroll_key_file.rs`)
  - Already emits `[wait]` for credential verify (good).
  - The actual `cryptsetup luksAddKey` (slot-1 keyfile enrollment)
    runs Argon2 again and is currently un-announced.

- **`braid unlock`** -- residual:
  - `btrfs device scan` is now subsumed under the new mount-phase
    `[wait]`; no separate row needed.
