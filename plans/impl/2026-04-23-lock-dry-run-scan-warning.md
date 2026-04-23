# Plan: dry-run `braid lock` mirrors real-run warning on /dev/mapper scan failure

## Context

`cli/src/lock.rs` has two scan-for-orphans code paths. The real-run path at
`cli/src/lock.rs:249-286` matches on `fs.list_dir("/dev/mapper")` and prints:

```
[warn]  could not scan /dev/mapper for orphans: {e} (skipping)
```

when `list_dir` returns `Err`. The dry-run path at `cli/src/lock.rs:149-162`
uses `if let Ok(entries) = fs.list_dir("/dev/mapper") { ... }`, silently
swallowing the same error and producing an empty orphan list.

Result: a user running `braid lock --dry-run` on a system where `/dev/mapper`
is unreadable (e.g. restricted container, permission issue) sees either
"nothing to do" or a preview missing any orphan entries, with no indication
anything went wrong. The subsequent real `braid lock` prints the `[warn]`
line, which is a surprise. This violates the dry-run contract of "preview
what the real command will do." Low severity -- no data-correctness risk,
pure UX mismatch.

## Fix

Make the dry-run helper the full preview boundary. Introduce:

```rust
pub fn render_lock_dry_run<F: Filesystem + ?Sized>(
    pool_was_mounted: bool,
    fs: &F,
    membership: &PoolMembership,
    mount_point: &MountPoint,
) -> String
```

in `cli/src/lock.rs`, adjacent to the existing `compile_lock_steps`. The
helper returns the complete preview text exactly as `braid lock --dry-run`
should emit it, including:

1. A `[warn]  could not scan /dev/mapper for orphans: {e} (skipping)` line
   when `fs.list_dir("/dev/mapper")` returns `Err`.
2. The step block rendered via the existing `Step::render_dry_run`
   (`cli/src/cmd.rs:256`), compiled from `compile_lock_steps`.
3. The literal `nothing to do.\n` when no steps are produced.

Rewire the `cmd_lock` dry-run branch (`cli/src/lock.rs:141-175`) to a thin
printer:

```rust
if dry_run {
    print!("{}", render_lock_dry_run(pool_was_mounted, fs, membership, mount_point));
    return Ok(());
}
```

Stream routing note: today the dry-run branch mixes streams (warn/`nothing
to do.` on stderr via `eprintln!`, steps on stdout via `print!`). Unifying
on stdout is an intentional incidental cleanup -- dry-run output is the
command's result, and a single stream makes the preview redirectable with
`braid lock --dry-run > preview.txt`. The pool-already-locked and
unmount-succeeded status lines emitted during the real run stay on stderr;
only the dry-run preview moves to stdout.

The real-run orphan scan at `cli/src/lock.rs:249-286` is **not** changed.
Its `[warn]` line uses the same message body as the dry-run helper; the
string is duplicated across the two sites. A single shared formatter
helper is not worth the indirection for a one-line message, and byte-
for-byte parity is enforced by the helper test below.

## Tests

Three Rust unit tests added to the existing `#[cfg(test)] mod tests`
block of `cli/src/lock.rs` (all call `render_lock_dry_run` directly and
assert on the returned string), plus one VM subtest in
`tests/cli/braid-lock.py` that pins the stdout-only stream contract at
the CLI boundary.

### 1. `dry_run_preview_warns_when_list_dir_fails`

Uses a local `FailListDirFs` stub identical in shape to the one inside
`lock_orphan_scan_failure_is_nonfatal` at `cli/src/lock.rs:873-894`, with
`list_dir` returning `std::io::Error::new(ErrorKind::PermissionDenied,
"permission denied")` (same construction the existing test uses, so the
Display text is stable and known). Scenario: pool not mounted, two
membership mappers visible via `fs.exists`, `list_dir` fails.

Assertions on the returned string:
- First line is the exact string `[warn]  could not scan /dev/mapper for orphans: permission denied (skipping)`
  (double space after `[warn]`, matching the real-run format at
  `cli/src/lock.rs:284`). Asserted via
  `assert!(output.starts_with("[warn]  could not scan /dev/mapper for orphans: permission denied (skipping)\n"))`
  so drift in prefix, body, spacing, or suffix fails the test.
- Contains both membership mapper close steps (`braid-aaa`, `braid-bbb`)
  from the rendered step block, proving the helper still compiled steps
  despite the scan failure.

This fails if: the warn line is dropped, the prefix changes, the message
body changes, or the helper short-circuits the step compile when
`list_dir` errors. Because the assertion pins the full line, it also
fails if the dry-run text drifts from the real-run text at
`cli/src/lock.rs:284` -- byte-for-byte parity is enforced by the test,
not by review discipline.

### 2. `dry_run_preview_mounted_happy_path`

Uses the existing `MockFs::new(&[...])` pattern (`cli/src/lock.rs:357`)
seeded with `/dev/mapper/braid-aaa`, `/dev/mapper/braid-bbb`, and one
orphan `/dev/mapper/braid-ccc`. Scenario: pool mounted, both membership
mappers open, one orphan present.

Assertions on the returned string:
- Does **not** contain `[warn]` (no scan failure).
- Contains `unmount /mnt/storage` and `btrfs device scan --forget`.
- Contains `close LUKS mapper braid-aaa` and `close LUKS mapper braid-bbb`.
- Contains `close LUKS mapper braid-ccc (orphan)`.

This pins that the helper renders happy-path previews correctly, so
refactoring the scan logic can't silently drop mappers or orphan steps
while only `compile_lock_steps`' separate tests stay green.

### 3. `dry_run_preview_nothing_to_do`

Scenario: pool not mounted, no membership mappers open, `list_dir` returns
`Ok(vec![])` (no orphan entries). Uses `MockFs::new(&[])`.

Assertion:
- `assert_eq!(render_lock_dry_run(false, &MockFs::new(&[]), &test_membership(),
  &MountPoint("/mnt/storage".into())), "nothing to do.\n");`

Pins the helper's no-op branch exactly -- altering or dropping the
`nothing to do.` line fails the test. The existing `dry_run_lock_nothing_to_do`
at `cli/src/lock.rs:1396` only asserts `compile_lock_steps` returns an
empty vec; it does not exercise the preview boundary's literal output.

### 4. VM subtest in `tests/cli/braid-lock.py`: `dry-run preview goes to stdout`

Added after the existing happy-path / idempotent coverage in
`tests/cli/braid-lock.py` (the file is already registered in `flake.nix`
via `checks.braid-lock`, so no new flake wiring is required).

Scenario: after a `braid lock` has run and the pool is fully locked
(already covered upstream in the file), run `braid lock --dry-run` and
capture the two streams separately:

```python
machine.succeed(
    "braid lock --dry-run >/tmp/lock-stdout 2>/tmp/lock-stderr"
)
stdout = machine.succeed("cat /tmp/lock-stdout")
stderr = machine.succeed("cat /tmp/lock-stderr")
assert stdout == "nothing to do.\n", f"unexpected stdout: {stdout!r}"
assert stderr == "", f"expected empty stderr, got: {stderr!r}"
```

Pins the stream-routing contract the fix establishes: the dry-run
preview is redirectable via `> preview.txt`. Reverting to the mixed
`eprintln!` routing (or regressing `print!` back to `eprintln!`) makes
this subtest fail.

The "nothing to do." form is used because it is the shortest
deterministic preview that exercises the helper end-to-end through the
real CLI binary; the Rust-level tests already cover the warn and step
branches of `render_lock_dry_run`.

### Existing tests kept

- `lock_orphan_scan_failure_is_nonfatal` (`cli/src/lock.rs:871`) continues
  to pin real-run nonfatal behavior.
- `dry_run_render_lock_mounted_2_disks`, `dry_run_lock_not_mounted_1_open`,
  `dry_run_lock_nothing_to_do` continue to pin `compile_lock_steps`
  directly. These tests stay valid; the new helper-level tests cover the
  integration of scan + compile that those lower tests miss.

## Critical files

- `cli/src/lock.rs` -- add `render_lock_dry_run` helper, collapse the
  `cmd_lock` dry-run branch to a single `print!`, add three unit tests.
- `tests/cli/braid-lock.py` -- add one VM subtest pinning the stdout-only
  stream contract.

## Verification

1. `cargo test -p braid-cli lock` -- all three new unit tests pass; all
   existing lock tests continue to pass.
2. `cargo test -p braid-cli` -- full crate suite green.
3. `just test-vm braid-lock` -- the existing suite plus the new
   stdout/stderr subtest pass end-to-end.
4. Manual parity diff: the real-run warning at `cli/src/lock.rs:284` is
   not covered by an automated test. Inspect the diff and confirm the
   dry-run warn body in `render_lock_dry_run` matches the real-run
   `eprintln!` call byte-for-byte (same words, same `(skipping)` suffix).
   Test #1 will fail if the dry-run side drifts, but the real-run side
   could drift independently; a quick grep pairs them during review.
