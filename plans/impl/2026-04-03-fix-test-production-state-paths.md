# Fix tests using production StatePaths

## Context

`just test-rust` fails on NixOS where `/var/lib/braid/` exists with root-only permissions. 11 tests fail with "Permission denied (os error 13)" when trying to read `pending-op.json`, alert latch files, or LUKS header directories under `/var/lib/braid/`. Tests pass on macOS only because the directory doesn't exist (reads return `NotFound` which is handled gracefully).

Tests should never depend on host filesystem state. Every test that uses `StatePaths::production()` or `StatePaths::custom("/var/lib/braid".into())` is latently broken.

## Plan

Replace all test-only uses of `StatePaths::production()` and `StatePaths::custom("/var/lib/braid".into())` with tempdir-backed paths. Per-file `test_paths()` helpers return `(TempDir, StatePaths)` to keep the tempdir alive.

### Per-file changes

**`cli/src/status.rs`** — 7 occurrences (lines 1623, 2783, 2806, 2816, 2826, 2924, 3003)

Add a `test_paths()` helper to the existing `mod tests` block:
```rust
fn test_paths() -> (tempfile::TempDir, StatePaths) {
    let tmp = tempfile::TempDir::new().unwrap();
    let paths = StatePaths::custom(tmp.path().into());
    (tmp, paths)
}
```
Replace each `&StatePaths::production()` with `let (_tmp, paths) = test_paths();` then `&paths`. The `_tmp` binding keeps the tempdir alive for the test's duration.

**`cli/src/unlock.rs`** — 6 occurrences (lines 311, 423, 559, 718, 790, 859)

Same pattern — add `test_paths()` helper, replace `&StatePaths::production()` in each `UnlockParams`.

**`cli/src/add.rs`** — `test_paths()` at line 815 returns `StatePaths::custom("/var/lib/braid".into())`

Change to return `(tempfile::TempDir, StatePaths)` and fix all call sites. The one test at line 940 that uses `StatePaths::production()` should also use this helper.

**`cli/src/replace.rs`** — `test_paths()` at line 727 returns `StatePaths::custom("/var/lib/braid".into())`

Same fix as `add.rs`.

**`cli/src/doctor.rs`** — ~20 occurrences

Already has `isolated_paths()` helper (line 549-552) that returns `(TempDir, StatePaths)`. Replace all `&StatePaths::production()` with `let (_dir, paths) = isolated_paths();` then `&paths`.

**`cli/src/enroll_key_file.rs`** — 3 occurrences (lines 907, 1068, 1096)

Add `test_paths()` helper returning `(TempDir, StatePaths)`, replace all three.

**`cli/src/tui/probe.rs`** — 1 occurrence (line 324, test code)

Add `test_paths()` helper, replace `&StatePaths::production()`.

### Not changed

- `cli/src/main.rs` (lines 241, 672) — production code, not tests
- `cli/src/state_paths.rs` (line 50) — tests path string equality, doesn't do I/O
- `cli/src/tui/model.rs` (line 178) — `Model::new_demo()` is production runtime code used by `braid --demo`, not a test. Keep on `StatePaths::production()`.

## Verification

`just test-rust` passes on both macOS and NixOS.
