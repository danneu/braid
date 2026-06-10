# plan: drop discover's vestigial `ByIdResolver` seam

## Context

A Low/Simplicity finding flagged that `discover` enumerates `/dev/disk/by-id`
with `std::fs::read_dir` directly while only using the injected `ByIdResolver`
for `canonicalize`, so the trait's `list_by_id_entries` method is bypassed --
"two ways to enumerate by-id." It proposed a doc comment justifying the
divergence by "sorting raw `DirEntry::file_name` values before canonicalization."

That justification is wrong (sorting works identically on the `Vec<String>` that
`list_by_id_entries` returns; collision-report determinism comes from
`discover.rs#label_collision` sorting its two paths, not from the enumeration
method). The real situation is structural: `RealByIdResolver::list_by_id_entries`
is hardcoded to `/dev/disk/by-id` with no directory parameter, while discover's
seam is an **injectable `by_id_dir: &Path`** pointed at a tempdir of real
symlinks. Investigation showed discover's `resolver` parameter is **vestigial**:

- All 21 `discover_from_dir(...)` call sites (1 production + 20 test) pass
  `&RealByIdResolver` -- it is never substituted with a mock.
- Discover uses the resolver only for `.canonicalize()`, which is exactly
  `std::fs::canonicalize` (`by_id.rs#RealByIdResolver`).
- Even the `CannotCanonicalize` failure path is exercised with **real dangling
  symlinks** in a tempdir, not a mocked resolver error.

So the trait abstraction buys discover zero test-substitution value. The ideal
fix is the opposite of documenting the split: **remove discover's
`ByIdResolver` dependency and call `std::fs::canonicalize` directly** (mirroring
its existing direct `std::fs::read_dir`), confining the `ByIdResolver` trait to
`recover.rs` -- its only real, mock-substituting consumer. This reverses only the
discover-half of the param-threading that
[`plans/impl/2026-05-21-extract-by-id-module.md`](../impl/2026-05-21-extract-by-id-module.md)
preserved for behavior-compat; the shared helpers (`by_id_priority`,
`is_partition_entry`) stay shared. Outcome: one enumeration story per command
(discover = direct `std::fs` against an injectable dir; recover = mockable
trait), and the reviewer confusion that generated the finding is dissolved at the
root rather than annotated.

## Goal / Non-goals

**Goal:** A behavior-preserving refactor that deletes the never-substituted
`ByIdResolver` thread from discover and documents the resulting boundary so the
finding cannot recur.

**Non-goals:**
- No change to `recover.rs`, `main.rs`, or `lib.rs` -- `ByIdResolver` /
  `RealByIdResolver` stay `pub` and are still used by recover (prod via
  `main.rs#main` constructing `RealByIdResolver`, tests via `MockByIdResolver`).
- Do **not** merge `discover_from_dir` into `discover_from_dir_inner` -- the
  wrapper exists to attach accumulated warnings on the error path
  (`DiscoverScan`), an independent concern (decision 022 report shape).
- Do **not** add a `dir` parameter to `list_by_id_entries` to route discover
  through the trait. That *increases* coupling and forces discover's
  real-symlink tests through an abstraction it would still never mock --
  strictly worse than removal.
- No new tests (see Tests); no behavior change; no fixture refresh (no
  parser/output change).

## Implementation

All edits are confined to two files: `cli/src/discover.rs` and
`cli/src/by_id.rs`.

### 1. Drop the resolver parameter from discover's three functions (`discover.rs`)

- `discover.rs#discover_pool_members`: change the body from
  `discover_from_dir(runner, &RealByIdResolver, Path::new("/dev/disk/by-id"))`
  to `discover_from_dir(runner, Path::new("/dev/disk/by-id"))`.
- `discover.rs#discover_from_dir`: remove the `resolver: &dyn ByIdResolver`
  parameter; update its call to `discover_from_dir_inner(runner, by_id_dir,
  &mut warnings)`.
- `discover.rs#discover_from_dir_inner`: remove the `resolver: &dyn
  ByIdResolver` parameter.

### 2. Canonicalize directly via `std::fs` (`discover.rs#discover_from_dir_inner`)

At the canonicalize site (the "Catch stale udev by-id symlinks before the LUKS
probe" block), replace the resolver call with `std::fs`, matching what
`by_id.rs#RealByIdResolver` did:

```rust
let canonical = match std::fs::canonicalize(&path_str) {
    Ok(c) => c.to_string_lossy().into_owned(),
    Err(e) => {
        warnings.push(DiscoverWarning::CannotCanonicalize {
            path: path_str.clone(),
            detail: e.to_string(),
        });
        continue;
    }
};
```

Only the `Ok` arm changes (add `.to_string_lossy().into_owned()`, since
`std::fs::canonicalize` yields `PathBuf` not `String`); the warning arm is
byte-identical. This mirrors the existing fully-qualified `std::fs::read_dir`
idiom already used a few lines above. Keep the existing explanatory comment about
catching stale udev symlinks.

### 3. Add a site comment that preempts the finding (`discover.rs#discover_from_dir_inner`)

At the `std::fs::read_dir(by_id_dir)` site, add a short ASCII comment stating the
deliberate design -- this is the finding's original (reasonable) ask, now with
the correct rationale, placed where a reader trips on it:

```rust
// Discover reads and canonicalizes its by-id directory directly via `std::fs`
// against an injectable `by_id_dir`, so its tests drive real udev-style
// symlinks in a tempdir (dangling ones included). The `ByIdResolver` trait is
// recover's mockable seam; discover does not need it.
```

### 4. Trim the import (`discover.rs`, first `use`)

Change `use crate::by_id::{ByIdResolver, RealByIdResolver, by_id_priority,
is_partition_entry};` to `use crate::by_id::{by_id_priority,
is_partition_entry};`. Both removed symbols are now unreferenced in `discover.rs`
(prod and tests); the two kept helpers are still called
(`discover.rs#discover_from_dir_inner` uses `is_partition_entry` and
`by_id_priority`).

### 5. Remove `&RealByIdResolver` from the test call sites (`discover.rs` tests)

`grep -n 'discover_from_dir(' cli/src/discover.rs` enumerates all 21 calls: the
1 production call (step 1) plus 20 in the `#[cfg(test)] mod tests` block -- sweep
exactly the sites the grep reports rather than chasing a fixed count. Every test
call -- `discover_from_dir(&runner, &RealByIdResolver, dir.path())` (and the two
`&IsLuksFailRunner` / `&LuksDumpFailRunner` variants) -- becomes
`discover_from_dir(&runner, dir.path())`. Mechanical drop-the-middle-arg across
all of them. No test body, assertion, or fixture changes -- the tempdir +
real-symlink helpers (`test_fixtures/discover.rs#discover_create_target`,
`#discover_create_by_id_symlink`) are untouched.

### 6. Refresh the boundary docs (`by_id.rs`)

The `ByIdResolver` trait (enumeration + canonicalization) becomes recover-only,
but `by_id_priority` / `is_partition_entry` stay shared by discover. Reword the
module doc and trait doc to say exactly that (and to record *why* discover
abstains, sealing the finding):

- Module `//!` doc: keep "Shared `/dev/disk/by-id/` symlink handling," but split
  the audiences accurately -- the prefix-priority and partition-filtering helpers
  serve both discover and recover; the `ByIdResolver` trait serves recover, which
  needs a mockable seam, whereas discover reads its injectable by-id directory
  directly via `std::fs`.
- `by_id.rs#ByIdResolver` trait doc: change "Resolve `/dev/disk/by-id/` symlinks
  for discover and recover." to recover-only, and note discover does not use the
  trait (it reads its injectable dir directly), keeping the existing
  "kept separate from `probe::Filesystem`" rationale.

Leave the `by_id_priority_ordering` test comment ("discover would silently prefer
the less stable symlink") as-is -- discover still uses `by_id_priority`, so it
remains accurate.

## Critical files

- Edited: `cli/src/discover.rs` (signatures, canonicalize site, read_dir
  comment, import, 20 test call sites)
- Edited: `cli/src/by_id.rs` (module + trait doc wording only)
- Untouched but verified: `cli/src/recover.rs`, `cli/src/main.rs`,
  `cli/src/lib.rs`, `cli/src/test_fixtures/discover.rs`

## Tests

No new tests. This is a pure code-organization change with no behavioral delta:
`std::fs::canonicalize` is precisely what `RealByIdResolver::canonicalize`
delegated to, and discover already drove it through real filesystem state. The
existing real-filesystem tests are the regression guard and must pass unchanged
(minus the dropped argument), in particular:

- `discover.rs` canonicalize-failure tests using real dangling symlinks
  (`discover_skips_entry_when_canonicalize_fails`,
  `discover_warns_on_dangling_symlink_with_no_luks_device`) -- prove the
  `CannotCanonicalize` path still fires via `std::fs`.
- The alias-dedup / priority / collision tests
  (`discover_prefers_wwn_over_ata`,
  `discover_same_priority_breaks_ties_lexicographically`,
  `discover_fails_on_label_collision_across_disks`) -- prove sort/dedup behavior
  is unchanged.
- All recover tests exercising `MockByIdResolver` / `list_by_id_entries` -- prove
  the trait still works for its remaining consumer.

## Verification

1. `cargo build -p braid-cli` -- compiles.
2. `cargo clippy -p braid-cli --all-targets` -- no unused-import or dead-code
   warnings (confirms `ByIdResolver`/`RealByIdResolver` are still reachable from
   recover + main, and discover's trimmed import is clean).
3. `just test-rust` -- the full CLI unit-test suite passes unchanged, including
   the discover and recover tests listed above.
4. `cargo fmt` -- formatting (test call-site edits and the doc rewrap).
5. Spot-checks:
   - `grep -n "ByIdResolver\|RealByIdResolver" cli/src/discover.rs` -> **no
     matches** (discover fully divorced from the trait).
   - `grep -n "ByIdResolver" cli/src/recover.rs cli/src/main.rs` -> still present
     (the trait kept its real home).

## Out of scope

- Any change to recover's use of the trait, or to `MockByIdResolver` /
  `resolver_for`.
- Merging the `discover_from_dir` / `_inner` split.
- Adding a directory parameter to `list_by_id_entries` (rejected: increases
  coupling for no test-substitution gain).
- Touching `by_id_priority` / `is_partition_entry`, which remain shared helpers.
