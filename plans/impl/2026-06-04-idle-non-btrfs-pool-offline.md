# Pin `cmd_idle` PoolOffline on a non-btrfs mount at the target

## Context

`braid idle` is the autosuspend gate: `PoolOffline` and `Idle` allow suspend
(exit 0), `Busy` blocks it. Step 1 of `cmd_idle` (`cli/src/idle.rs#cmd_idle`)
gates on `mount_check::is_btrfs_mounted`, which is defined as
`fstype_at_mount(...) == Some("btrfs")`. That collapses two distinct mountinfo
inputs into one `Ok(false)` -> `PoolOffline` outcome:

1. nothing mounted at the target (`fstype` = `None`), and
2. a **non-btrfs** filesystem mounted at the target (`fstype` = `Some("ext4")`).

This is a **deliberate divergence** from the three `probe_*` entry points
(`cli/src/probe.rs#probe_pool`, `#probe_fsid`, `#probe_pool_alerts`), which all
match `Some(fstype) if fstype != "btrfs"` and return `ProbeError::NotBtrfs`. The
idle behavior is correct: an ext4 fs at `/mnt/storage` means the encrypted btrfs
pool is not assembled, so there is nothing to protect and suspend is safe.

The gap: that divergence has **no regression guard**. The existing sibling test
`idle_when_pool_offline` (`cli/src/idle.rs`) only covers sub-case (1) -- its
fixture (`offline_mountinfo` -> `MOUNTINFO_WITHOUT_TARGET`) puts ext4 at `/` with
*nothing* at `/mnt/storage`, so `fstype` is `None` and the non-btrfs branch is
never exercised. The parser layer pins the distinction
(`mount_check.rs` `fstype_at_mount_returns_other_fstype`), but the `cmd_idle`
wiring of "ext4 at target -> PoolOffline" is unpinned. A refactor that swapped
`is_btrfs_mounted` for `fstype_at_mount_via_fs` + a `NotBtrfs`-style error -- to
align idle with `probe_*` -- would compile cleanly, keep every parser test green,
and silently flip this case from "suspend allowed" to "suspend blocked"/hard
error. This is exactly the silent-wiring-regression shape the existing
`idle_when_scrub_*` tests in this file already exist to catch.

Intended outcome: one behavioral, structure-insensitive unit test that pins the
intentional divergence, documented in its preamble.

## Approach

Test-only change, mirroring the existing `idle_when_pool_offline` sibling so the
two step-1 short-circuit tests read as a pair differing only in fixture + intent.
No production code changes (the behavior is already correct); no code comment in
`cmd_idle` (per this file's convention, the "why" for wiring lives in the test
preamble, as it does for the scrub-state wiring tests).

### 1. Fixture: named constructor for the non-btrfs-at-target scenario

In `cli/src/test_fixtures/idle.rs`, add a private const + associated fn next to
the existing `MOUNTINFO_*` consts and `offline_mountinfo`. Use a named
constructor (parallel to `offline_mountinfo`), **not** inline `with_mountinfo` --
`with_mountinfo` is documented for "bad input" parser-failure tests, whereas a
non-btrfs mount is a semantically valid scenario. The const mirrors
`MOUNTINFO_WITH_BTRFS_TARGET` with `btrfs` swapped for `ext4` (same mount point,
fstype is the variable under test):

```rust
const MOUNTINFO_NON_BTRFS_TARGET: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";

/// Mountinfo fixture with a non-btrfs filesystem at the configured target,
/// pinning that idle treats it as PoolOffline rather than diverging into the
/// probe_* NotBtrfs error.
pub(crate) fn non_btrfs_target() -> Self {
    Self::empty().seed_mountinfo(MOUNTINFO_NON_BTRFS_TARGET)
}
```

No re-export edit: `non_btrfs_target` is an associated fn on `IdleMockFs`, which
is already re-exported (`cli/src/test_fixtures.rs:163`). The const stays private,
like its `MOUNTINFO_*` siblings.

### 2. Test: pin PoolOffline through `cmd_idle`

In `cli/src/idle.rs`'s `mod tests`, add immediately after `idle_when_pool_offline`
(after line 138). Body is identical to that sibling except the fixture; the
full Intent/Why/Scenario preamble documents the deliberate divergence:

```rust
// Intent: a non-btrfs filesystem mounted at the configured mount point
//   yields PoolOffline (allow suspend), not Busy and not an error.
// Why it exists: cmd_idle gates on is_btrfs_mounted, which collapses
//   "nothing mounted" and "non-btrfs mounted" into one Ok(false) ->
//   PoolOffline. This deliberately diverges from probe_pool / probe_fsid /
//   probe_pool_alerts, which reject a non-btrfs mount at the same path with
//   ProbeError::NotBtrfs. The divergence is correct (ext4 at /mnt/storage
//   means the btrfs pool is not assembled, so suspend is safe) but was
//   unguarded: a refactor swapping is_btrfs_mounted for fstype_at_mount_via_fs
//   + a NotBtrfs-style error would compile, keep parser tests green, and
//   silently flip this case to suspend-blocked. The sibling
//   idle_when_pool_offline only covers the unmounted case (fstype None),
//   which never exercises this branch.
// Scenario: a misconfiguration mounts ext4 at /mnt/storage; autosuspend must
//   still be allowed because the encrypted btrfs pool is offline.
#[test]
fn pool_offline_when_non_btrfs_at_mount_point() {
    let runner = MockRunner::default();
    let fs = IdleMockFs::non_btrfs_target();
    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_eq!(result, IdleResult::PoolOffline);
}
```

`MockRunner::default()` (no scrub seed) and the fixture's mountinfo-only surface
(no `/sys/fs/btrfs` listing) mean any regression that fails to short-circuit at
step 1 reaches the sysfs scan or scrub probe and returns `Busy::Unknown` --
failing the `PoolOffline` assertion. So asserting `PoolOffline` alone proves the
short-circuit; matching `idle_when_pool_offline`, no separate
`runner.requests().is_empty()` assertion is added (keeps the sibling pair
identical in shape).

## Critical files

- `cli/src/test_fixtures/idle.rs` -- add `MOUNTINFO_NON_BTRFS_TARGET` const and
  `IdleMockFs::non_btrfs_target()` (next to `offline_mountinfo`).
- `cli/src/idle.rs` -- add `pool_offline_when_non_btrfs_at_mount_point` test after
  `idle_when_pool_offline`.

## Reuse

- `IdleMockFs` + `seed_mountinfo` / `empty` (`cli/src/test_fixtures/idle.rs`) --
  the new constructor composes them exactly like `offline_mountinfo`.
- `idle_mp()` (`cli/src/test_fixtures/idle.rs`) -- canonical `/mnt/storage`.
- `idle_when_pool_offline` (`cli/src/idle.rs`) -- structural template for the
  new test.
- Pattern precedent for "pin cmd_idle wiring a parser test does not cover":
  the `idle_when_scrub_never` / `_aborted` / `_interrupted` cluster in the same
  file.

## Verification

Focused run of the new test -- from the repo (workspace) root, where `--lib`
resolves the single `braid-cli` member, mirroring how the `test-rust` recipe
itself invokes cargo:

```
cargo test --lib pool_offline_when_non_btrfs_at_mount_point
```

Full Rust verification. The `test-rust` recipe (`Justfile`) takes no test-name
argument, so `just test-rust <name>` is not a valid invocation -- the focused
filter above must go through `cargo` directly:

```
just test-rust
```

To prove the test actually guards the divergence (optional, local only -- do not
commit): temporarily edit `cmd_idle` step 1 to treat a non-btrfs mount as an
error/Busy (e.g. switch to `fstype_at_mount_via_fs` and `busy_unknown` on
`Some(ft != "btrfs")`); the new test must fail while `idle_when_pool_offline`
still passes, demonstrating the gap is real and now covered. Revert the temporary
edit. No VM tests needed -- this is a pure Rust unit-test addition.
