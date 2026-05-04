# Fix: discover silently collapses two distinct disks sharing a `braid-<name>` label

## Context

`cli/src/discover.rs::discover_from_dir` walks `/dev/disk/by-id/` and builds a
`BTreeMap<String, ByIdPath>` keyed by the disk name extracted from each LUKS
label `braid-<name>`. When the same `disk_name` appears twice in the loop, the
`Entry::Occupied` arm at lines 93-103 picks the priority winner (`wwn-` over
`ata-`, then lexicographic) and silently drops the other.

That tie-break is correct for the legitimate case: a single physical disk
exposed under multiple aliases (e.g. `/dev/disk/by-id/wwn-0xABCD` and
`/dev/disk/by-id/ata-SEAGATE_X` both resolving to `/dev/sda`). It is **wrong**
when two physically distinct devices both carry the same `braid-<name>` label
-- which happens after a `dd` clone or a manual mislabel. In that case
`braid discover --write` writes only one of them to `pool.json`, the next
`braid unlock` opens that single disk, and btrfs assembles the array with a
member missing -- either silently degraded (RAID1 on >2 disks) or refusing to
mount (RAID1 on 2 disks plus degraded refusal). The user gets no warning that
a second disk was ignored.

The fix is to canonicalize each by-id symlink to its kernel device path. Two
symlinks aliasing the same physical disk collapse to the same canonical path
(legitimate -- pick the priority winner as today). Two symlinks resolving to
*different* canonical paths under the same `braid-<name>` label is an
ambiguous-input state braid cannot safely recover from: fail the scan with
an actionable error so the user can relabel or detach one disk before
retrying.

## Approach

Reuse the existing `crate::recover::ByIdResolver` trait
(`cli/src/recover.rs:37-44`); do not introduce a new abstraction. Keep the
public `discover_pool_members(runner)` signature unchanged (it constructs
`RealByIdResolver` internally). Add a resolver parameter to the test-only
`discover_from_dir`. Track each accepted entry's canonical path alongside its
`ByIdPath`; on a second sight of the same `disk_name`, compare canonical
paths and either tie-break (same canonical) or fail (different canonical).

Refactor the existing test fixtures from plain `tempfile` files to real
symlinks against shared targets, and pass `RealByIdResolver` into
`discover_from_dir` so canonicalization actually runs against the tempdir.
The precedent for symlinks in tests is `cli/src/tui/probe.rs` around lines
1557-1609.

## Critical files

- `cli/src/discover.rs` -- error variant, signature change, collision
  detection, test rewrite, new collision test.
- No changes to `cli/src/main.rs` (the public `discover_pool_members` shape
  is preserved).
- No changes to `cli/src/recover.rs` (the `ByIdResolver` trait already
  exposes the needed `canonicalize` method).

## Implementation steps

### 1. Add a new variant to `DiscoverError`

`cli/src/discover.rs:8-14`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("failed to read /dev/disk/by-id: {0}")]
    ReadDir(#[source] std::io::Error),
    #[error("label collision: braid-{name} found on two distinct devices ({path1}, {path2}) -- relabel or detach one before retrying")]
    LabelCollision {
        name: String,
        path1: String,
        path2: String,
    },
}
```

The error reports the by-id paths (what the user edits/detaches), not the
canonical kernel paths. `--` not em-dash, lowercase, remediation hint after
`--`, matching the `ack.rs:209` voice. `print_cli_error` in `main.rs` prefixes
`error: ` automatically.

### 2. Change `discover_from_dir` signature

```rust
pub fn discover_pool_members<R: CommandRunner>(
    runner: &R,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    discover_from_dir(runner, &crate::recover::RealByIdResolver, Path::new("/dev/disk/by-id"))
}

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    resolver: &dyn crate::recover::ByIdResolver,
    by_id_dir: &Path,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    ...
}
```

Public API unchanged. No `main.rs` edit.

### 3. Replace the label-acceptance block

Inside the existing `if let Some(label) ... && is_valid_disk_name(...)` guard,
canonicalize the candidate path and either tie-break or fail. The local
`members` map gains a canonical-path field:

```rust
let mut members: BTreeMap<String, (ByIdPath, String)> = BTreeMap::new();
```

Add a small pure helper just above `discover_from_dir`. Extracting the
sort+pack step into a named helper lets a unit test pin the ordering
invariant directly (see test 6c) without depending on `read_dir` order:

```rust
/// Build a `LabelCollision` error from two colliding by-id paths.
/// Sorts the paths lexicographically so the error message and any
/// downstream logging are deterministic across read_dir orderings.
fn label_collision(name: &str, a: String, b: String) -> DiscoverError {
    let mut paths = [a, b];
    paths.sort();
    let [path1, path2] = paths;
    DiscoverError::LabelCollision {
        name: name.to_owned(),
        path1,
        path2,
    }
}
```

Replacement for `discover.rs:85-104`:

```rust
if let Some(label) = label
    && let Some(disk_name) = crate::config::name_from_mapper(&label)
    && crate::membership::is_valid_disk_name(disk_name)
{
    // Canonicalize each same-label candidate first, then decide:
    // matching canonical paths = two aliases of one physical disk, fall
    // through to the priority/filename tie-break. Mismatched canonical
    // paths = two physically distinct disks share a braid-<name> label,
    // which braid cannot resolve automatically -- fail loud.
    let canonical = match resolver.canonicalize(&path_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: skipping {path_str}: cannot canonicalize: {e}");
            continue;
        }
    };

    match members.entry(disk_name.to_owned()) {
        Entry::Vacant(e) => {
            e.insert((ByIdPath(path_str), canonical));
        }
        Entry::Occupied(mut e) => {
            let (existing_by_id, existing_canonical) = e.get();
            if *existing_canonical != canonical {
                return Err(label_collision(
                    disk_name,
                    existing_by_id.0.clone(),
                    path_str,
                ));
            }
            // Same physical disk via two aliases -- pick by (priority, filename).
            let existing_name =
                existing_by_id.0.rsplit('/').next().unwrap_or("").to_owned();
            let candidate_key = (by_id_priority(&name_str), name_str.as_ref());
            let existing_key = (by_id_priority(&existing_name), existing_name.as_str());
            if candidate_key < existing_key {
                e.insert((ByIdPath(path_str), canonical));
            }
        }
    }
}
```

Then project the augmented map back to the public return type at the end of
the function:

```rust
Ok(members.into_iter().map(|(k, (by_id, _))| (k, by_id)).collect())
```

The `paths.sort()` inside `label_collision` is required because `read_dir`
order is unspecified; without it the error message would be non-deterministic.
Test 6c pins this invariant deterministically by exercising the helper with
both encounter orderings.

### 4. Test helpers

Replace `create_file` (`discover.rs:249-253`) with two helpers, used uniformly
across every test that calls `discover_from_dir`:

```rust
/// Create a real placeholder file in `dir` representing a physical
/// device. Symlinks pointing at this file canonicalize to its path.
fn create_target(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    path.to_string_lossy().into_owned()
}

/// Create a by-id symlink in `dir` pointing at `target`. Returns the
/// symlink's full path, which is what discover_from_dir/cryptsetup
/// see at runtime.
fn create_by_id_symlink(dir: &Path, name: &str, target: &str) -> String {
    let path = dir.join(name);
    std::os::unix::fs::symlink(target, &path).unwrap();
    path.to_string_lossy().into_owned()
}
```

`create_target` returns the un-canonicalized path. `RealByIdResolver` calls
`std::fs::canonicalize` on each symlink path; on macOS this resolves
`/var/folders/.../fake-sda` through `/private/var/...`, but both symlinks to
the same target collapse to the same final string, so equality holds.

### 5. Rewrite affected tests

All five `discover_from_dir` callers now pass `&crate::recover::RealByIdResolver`
and use real symlinks:

- `non_luks_device_never_reaches_luks_dump` (line 256): one target per by-id
  entry (no aliasing in this test). Just makes them symlinks for uniformity.
- `discover_prefers_wwn_over_ata` (line 312): one target `fake-sda`; both
  `ata-SEAGATE_ST500` and `wwn-0x50014ee606704442` symlink to it. Asserts the
  wwn symlink wins.
- `discover_same_priority_breaks_ties_lexicographically` (line 337): one
  target `fake-sda`; `ata-ZZZZZ_DISK` and `ata-AAAAA_DISK` both symlink to
  it. Asserts the alphabetically earlier symlink wins.
- `discover_skips_luks1_disk` (line 362): one target per by-id (no aliasing).
- `discover_selects_best_symlink_per_disk_independently` (line 400): two
  targets `fake-disk1` and `fake-disk2`; the alpha pair symlinks to the
  former, the beta pair to the latter. Asserts wwn wins for each disk.

`partition_detection` and `by_id_priority_ordering` don't call
`discover_from_dir` -- no change.

### 6. New tests

Three new tests cover the collision branch end-to-end, the sibling warn-and-
skip branch, and the deterministic-ordering invariant on the helper directly.

#### 6a. Collision integration test

```rust
#[test]
fn discover_fails_on_label_collision_across_disks() {
    /*
     * Intent: two distinct physical devices that both carry the same
     *   braid-<name> LUKS label must produce a hard error from discover,
     *   not silent loser-drop. pool.json must never be writable in this
     *   ambiguous state.
     * Why it exists: the prior priority/tie-break logic silently dropped
     *   the loser when two by-id paths shared a disk_name, conflating
     *   "same disk, two aliases" with "two disks, same label". The latter
     *   happens after a `dd` clone or a manual mislabel; persisting one
     *   to pool.json then unlocking with a member short would assemble
     *   btrfs degraded or refuse to mount.
     * Scenario: admin clones a working braid disk to a spare for
     *   migration testing and forgets to relabel; both disks present
     *   identical labels at the next `braid discover` run.
     */
    let dir = tempfile::tempdir().unwrap();
    let target_a = create_target(dir.path(), "fake-sda");
    let target_b = create_target(dir.path(), "fake-sdb");
    let alias_a = create_by_id_symlink(dir.path(), "ata-CLONE_A", &target_a);
    let alias_b = create_by_id_symlink(dir.path(), "ata-CLONE_B", &target_b);
    let runner = LabelMap::new(&[(&alias_a, "braid-foo"), (&alias_b, "braid-foo")]);

    let err = discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path())
        .unwrap_err();

    match &err {
        DiscoverError::LabelCollision { name, path1, path2 } => {
            assert_eq!(name, "foo");
            let pair = [path1.as_str(), path2.as_str()];
            assert!(
                pair.contains(&alias_a.as_str()) && pair.contains(&alias_b.as_str()),
                "collision must reference both aliases: {pair:?}",
            );
        }
        other => panic!("expected LabelCollision, got {other:?}"),
    }

    let msg = err.to_string();
    assert!(msg.contains("braid-foo"), "missing label name: {msg}");
    assert!(msg.contains(&alias_a), "missing alias_a: {msg}");
    assert!(msg.contains(&alias_b), "missing alias_b: {msg}");
}
```

This test pins the *behavioral* contract: two distinct devices with the same
label produce a `LabelCollision` that names both aliases. It does not pin
ordering -- ordering is owned by test 6c, which exercises both encounter
sequences deterministically.

#### 6b. Canonicalize-failure skip test

Uses a real dangling symlink (target path doesn't exist) so
`std::fs::canonicalize` returns `NotFound`. No mock resolver needed --
`RealByIdResolver` is the right surface to test against.

```rust
#[test]
fn discover_skips_entry_when_canonicalize_fails() {
    /*
     * Intent: a by-id symlink whose canonicalize errors (e.g. broken
     *   symlink, EACCES) is skipped with a warning -- it does not abort
     *   the scan and does not get treated as a collision-eligible peer.
     * Why it exists: the collision-detection branch added in
     *   discover_fails_on_label_collision_across_disks fires only when
     *   canonicalize SUCCEEDS for two entries with mismatched targets.
     *   Without this test, a regression that turns the warn-and-skip
     *   into a hard `LabelCollision` (or into a silent-accept that
     *   inserts a non-canonicalizable entry) could ship without notice.
     * Scenario: one of two braid disks has a broken /dev/disk/by-id
     *   symlink (e.g. udev hasn't repopulated after a transient detach);
     *   discover should still record the other disk's membership rather
     *   than failing the scan or claiming a collision.
     */
    let dir = tempfile::tempdir().unwrap();
    let target = create_target(dir.path(), "fake-sda");
    // Dangling symlink: target does not exist, so canonicalize returns Err.
    let dangling = create_by_id_symlink(
        dir.path(),
        "ata-DANGLING",
        "/nonexistent/dangling/target",
    );
    let valid = create_by_id_symlink(dir.path(), "wwn-VALID", &target);
    let runner = LabelMap::new(&[(&dangling, "braid-foo"), (&valid, "braid-foo")]);

    let members = discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path())
        .unwrap();

    assert_eq!(members.len(), 1, "expected only the canonicalizable entry");
    assert!(
        members["foo"].0.ends_with("wwn-VALID"),
        "expected the valid symlink to win, got: {}",
        members["foo"].0
    );
}
```

#### 6c. Helper unit test pins deterministic ordering

The integration test at 6a depends on `read_dir` ordering, which is
unspecified across filesystems. To catch a regression that drops the
`paths.sort()` inside `label_collision`, exercise the helper directly with
both encounter orderings -- one of them must be the unsorted input, so the
sort is observable.

```rust
#[test]
fn label_collision_sorts_paths_lexicographically() {
    /*
     * Intent: the LabelCollision error variant must report `path1` and
     *   `path2` in lexicographic order regardless of which path was
     *   encountered first during the by-id directory scan.
     * Why it exists: discover_fails_on_label_collision_across_disks asserts
     *   that the collision references both aliases, but its outcome depends
     *   on uncontrolled `read_dir` order; on filesystems that happen to
     *   return creation order or sorted order, removing `paths.sort()`
     *   from `label_collision` could pass that test by accident. Driving
     *   the helper with both orderings pins the invariant deterministically.
     * Scenario: collision detection produces the same error message whether
     *   the lexicographically-earlier path was the incumbent or the new
     *   candidate -- so users see a stable message between runs and
     *   reboots.
     */
    let a = "/dev/disk/by-id/ata-AAA".to_owned();
    let z = "/dev/disk/by-id/ata-ZZZ".to_owned();

    for (incumbent, candidate) in [(a.clone(), z.clone()), (z.clone(), a.clone())] {
        let err = label_collision("foo", incumbent.clone(), candidate.clone());
        match err {
            DiscoverError::LabelCollision { name, path1, path2 } => {
                assert_eq!(name, "foo");
                assert_eq!(path1, a, "(incumbent={incumbent}, candidate={candidate})");
                assert_eq!(path2, z, "(incumbent={incumbent}, candidate={candidate})");
            }
            other => panic!("expected LabelCollision, got {other:?}"),
        }
    }
}
```

### 7. Update `manual/commands/discover.md`

Keep the manual in sync with the new safety check (per the AGENTS.md rule
that user-facing docs track behavior changes). The wording must reflect the
actual ordering: canonicalization happens *per same-label candidate*
*before* the priority tie-break, and the tie-break is what runs in the
"same canonical device" sub-case. The collision check is what runs in the
"different canonical device" sub-case -- it does not happen "after picking
the best symlink".

Under "What happens under the hood", **rewrite** the existing step 8 and
**insert** a new step 9 (renumbering the existing step 9 to 10):

> 8. When multiple `/dev/disk/by-id/` symlinks resolve to the same
>    canonical kernel device (i.e. `wwn-` and `ata-` aliases of the same
>    physical disk), picks the most stable one (preference order:
>    wwn > nvme > scsi > ata > usb > other, with lexicographic
>    tie-breaking).
>
> 9. If two symlinks that share the same `braid-<name>` label resolve to
>    *different* kernel devices, refuses the entire scan with an error.
>    Two physically distinct disks share a label -- typically after a
>    `dd` clone or a manual mislabel -- and braid cannot safely choose
>    one. Relabel or detach one disk before retrying.
>
> 10. With `--write`, saves the discovered membership to `pool.json`.

Under "Safety checks", **add** a bullet:

> - Refuses the scan if two distinct devices share the same `braid-<name>`
>   LUKS label -- relabel or detach one disk before retrying.

## Edge cases (verified)

- **Broken symlink / EACCES on canonicalize**: `eprintln!` warning, `continue`.
  Matches the LUKS1-skip precedent (`discover.rs:76`). A symlink that doesn't
  resolve cannot be a pool member, and silencing the entry cannot mask a
  collision -- the collision branch only fires on two successful canonicalizes.
- **Symlink-to-symlink chain**: `std::fs::canonicalize` is `realpath(3)`-style
  -- it resolves the entire chain. No special handling.
- **Partition entries**: `is_partition_entry` filters at line 43, before any
  cryptsetup or canonicalize call. Unchanged.
- **Three-way collision**: the second occurrence triggers the error and
  returns. The third is never seen. The user resolves one collision, re-runs,
  sees the next. Don't enumerate all colliders -- the simpler pattern is
  fine and matches braid's "fix one thing, retry" UX in other commands.
- **Mixed legitimate-alias-plus-collision**: e.g. `wwn-X -> sda`,
  `ata-X -> sda`, `ata-Y -> sdb`, all labeled `braid-foo`. Read order varies;
  in any order, the third entry hits `Occupied` with a canonical mismatch and
  the collision fires. The reported `path1`/`path2` will be one of the sda
  aliases plus `ata-Y`, which still points the user at the right physical
  contradiction.

## Verification

1. `just test-rust` -- the five rewritten existing tests must still pass
   (proves the canonicalize wiring doesn't regress the legitimate-aliasing
   path). All three new tests must pass:
   - `discover_fails_on_label_collision_across_disks` -- hard-fail branch
     end-to-end against real symlinks.
   - `discover_skips_entry_when_canonicalize_fails` -- warn-and-skip branch
     against a real dangling symlink.
   - `label_collision_sorts_paths_lexicographically` -- pins deterministic
     ordering on the helper directly, independent of `read_dir` order.
2. `cargo check -p braid-cli` -- confirms no API leak; `main.rs` is
   untouched and `discover_pool_members` still takes only `&R`.
3. Manual diff: `manual/commands/discover.md` reflects the new collision
   refusal under "What happens under the hood" (steps 8-10 rewritten) and
   "Safety checks".
4. No VM test required: the change is pure logic on a pre-existing trait;
   the existing parser/cryptsetup contract is unchanged.

## What a reviewer should scrutinize

- **Cross-module dependency**: `discover.rs` now imports
  `crate::recover::ByIdResolver` and `crate::recover::RealByIdResolver`.
  `recover.rs` already references `discover::by_id_priority` and
  `discover::is_partition_entry`, so no new cycle. If a third caller of
  `ByIdResolver` ever appears, the trait may want to migrate to a
  dedicated `cli/src/by_id.rs`. Out of scope here.
- **macOS tempdir canonicalization quirk** (test note): canonicalize on
  `/var/folders/.../fake-sda` returns `/private/var/folders/.../fake-sda`.
  Both symlinks targeting the same un-canonicalized path still canonicalize
  to the same final string, so equality holds. The test asserts behavior
  via `LabelCollision` matching, not raw canonical-path equality, which
  keeps it portable between Linux CI and macOS dev.
- **Error string bakes `braid` and `braid-` prefix**: the message references
  `braid-{name}` (the LUKS label form) and the verb "retrying" (no command
  name). If the label prefix is ever changed, the message updates with it.
