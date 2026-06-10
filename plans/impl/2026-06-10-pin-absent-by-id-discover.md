# Plan: pin the absent-`/dev/disk/by-id` discover branch with a unit test

## Context

`discover_from_dir_inner` (`cli/src/discover.rs`) treats a **missing**
`/dev/disk/by-id` directory as "no members," returning `Ok(PoolMembership::empty())`
early on the `read_dir` `NotFound` arm:

```rust
let entries = match std::fs::read_dir(by_id_dir) {
    Ok(entries) => entries,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
        return Ok(PoolMembership::empty());      // <- this arm
    }
    Err(e) => return Err(DiscoverError::ReadDir(e)),
};
```

That empty membership is what the `Commands::Discover` arm
(`cli/src/main.rs#Commands::Discover`) routes to the remediation-bearing
`NoMembersDiscovered` refusal ("no braid-labeled LUKS2
devices found -- check that pool members are attached and readable ..."). A
regression that let an absent directory fall through to the generic
`Err(DiscoverError::ReadDir(e))` arm instead would replace that friendly refusal
with a raw `failed to read /dev/disk/by-id: ...` I/O error.

**This arm has no direct test.** Verification established:

- All 20 `discover_from_dir(...)` call sites in the test module pass a **live**
  tempdir; none pass an absent path. `discover_from_dir` is module-private, so this
  branch can only be unit-tested from inside `discover.rs`.
- `tests/cli/braid-discover-empty-scan.py` drives the real binary against the VM's
  real `/dev/disk/by-id`, which **exists** (populated by the VM's own disks). It
  therefore exercises the `Ok(entries)` loop-falls-through-empty path, not the
  `read_dir` `NotFound` early return.
- The sibling `classify_pool_json` (pool.json's loader) pins **both** of its
  `NotFound`-handling arms (`classify_pool_json_returns_missing_when_absent` and
  `classify_pool_json_returns_corrupt_for_non_not_found_io`). The by-id `read_dir`
  pins neither. This test closes the high-value, trivially-testable half.

Outcome: one focused unit test so the absent-directory -> empty-scan ->
`NoMembersDiscovered` contract cannot silently regress into a hard I/O error.

## Change

Add a single `#[test]` to the `mod tests` block in `cli/src/discover.rs`, reusing
the existing fixtures already imported there (`DiscoverLabelMap`, `RealByIdResolver`).
No production code changes; the behavior under test already exists and is correct.

The runner is never invoked: `read_dir` returns `NotFound` before any cryptsetup
probe, so the empty `DiscoverLabelMap::new(&[])` (the same idiom the dangling-symlink
test uses) suffices. Build the absent path as a guaranteed-nonexistent child of a
live tempdir.

```rust
// Intent: a missing /dev/disk/by-id directory yields an Ok(empty)
//   membership -- which the CLI routes to the NoMembersDiscovered refusal
//   -- not a hard DiscoverError::ReadDir I/O error.
// Why it exists: the read_dir NotFound arm is the only discover branch that
//   turns an absent by-id directory into an empty scan. The
//   braid-discover-empty-scan.py VM test only exercises a present directory
//   with no braid disks (the loop falls through empty), so a regression
//   collapsing this arm into the generic ReadDir error -- swapping the
//   remediation-bearing refusal for a raw I/O error -- would pass every
//   other test. Mirrors classify_pool_json's absent-vs-other-io test pair,
//   but for the by-id read_dir.
// Scenario: a minimal or early-boot host with no block devices exposing
//   by-id symlinks has no /dev/disk/by-id at all; `braid discover` must
//   refuse cleanly with the no-members message, not crash with an I/O error.
#[test]
fn discover_returns_empty_when_by_id_dir_absent() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("by-id-does-not-exist");
    let runner = DiscoverLabelMap::new(&[]);

    let scan = discover_from_dir(&runner, &RealByIdResolver, &absent);
    let members = scan
        .result
        .expect("absent by-id dir must yield Ok(empty), not DiscoverError::ReadDir");

    assert!(
        members.is_empty(),
        "absent by-id dir must yield an empty membership: {members:?}"
    );
    assert!(
        scan.warnings.is_empty(),
        "absent by-id dir must produce no warnings: {:?}",
        scan.warnings
    );
}
```

**Placement & naming.** Put it next to the other "by-id directory in an unusual
state" structural tests -- right after
`cli/src/discover.rs#discover_skips_entry_when_canonicalize_fails` -- grouping it
with `cli/src/discover.rs#discover_warns_on_dangling_symlink_with_no_luks_device`.
The name mirrors the sibling
`cli/src/discover.rs#classify_pool_json_returns_missing_when_absent` vocabulary
("absent").

**Style.** Follow `docs/dev/testing.md#preamble-literal-line-comment-form`: the
Intent / Why it exists / Scenario preamble is a contiguous block of `//` line
comments directly above the `#[test]` item -- not a `/* */` block inside the
function. (Some older tests in this module still use the block form, but the
documented convention is the `//` form, which a new test follows.) A `#[test] fn`
is not `pub`, so no `///` doc comment is required.

## Out of scope (deliberately)

The non-`NotFound` `Err(e) => DiscoverError::ReadDir(e)` arm (and the iterator-collect
`ReadDir` at the `.collect::<Result<Vec<_>, _>>()` site) are **not** unit-tested: a
non-`NotFound` `read_dir` failure (e.g. `EACCES`, or pointing at a non-directory)
can't be forced deterministically and portably in a unit test without root or a
special filesystem. The finding scoped to the `NotFound` arm for exactly this reason;
do not expand to demand the others.

## Verification

1. `just test-rust` -- the new test runs under `cargo test --lib` and must pass green
   (the production behavior already exists). Confirm with
   `cargo test --lib discover_returns_empty_when_by_id_dir_absent`.
2. **Confirm it actually pins the branch (TDD red-confirm).** Temporarily change the
   `NotFound` arm at `cli/src/discover.rs` to fall through to the generic error
   (e.g. delete the `Err(e) if e.kind() == NotFound => ...` arm so a missing dir hits
   `Err(e) => return Err(DiscoverError::ReadDir(e))`), re-run the test, and confirm it
   fails on the `.expect(...)` with a `ReadDir` error -- proving the test guards the
   exact regression described. Revert the change.
3. No VM lane needed: this is pure control flow over `read_dir`'s error kind, with no
   real device interaction -- the Rust unit lane is the correct and cheapest home.
