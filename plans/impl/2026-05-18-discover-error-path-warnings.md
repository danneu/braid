# Plan: Surface discover warnings on the error path

## Context

`cli/src/discover.rs::discover_from_dir` accumulates a `Vec<DiscoverWarning>`
as it scans `/dev/disk/by-id/` (dangling symlinks, LUKS1 disks, invalid
braid labels, missing/invalid LUKS UUIDs, unreadable `luksDump` output).
On the happy path those warnings flow out through `DiscoverOutcome.warnings`
and `main.rs` prints them to stderr before the preview.

Three early-`return Err(...)` sites silently drop the accumulated vec:

- `cli/src/discover.rs:451-455` -- `LabelCollision` inside the entry loop.
- `cli/src/discover.rs:485-491` -- `DuplicateUuid` after the post-loop
  UUID-dedup pass.
- `cli/src/discover.rs:502-518` -- the defense-in-depth
  `membership.insert(...)?` after dedup.

(Line numbers reflect the file state at planning time; the implementor
should match against the symbol, not the line.)

The first one is especially harmful: it exits the entry loop, so any
warning that *would have been produced by a later entry* is also lost,
not just the warnings already collected. With nondeterministic
`read_dir` order, a multi-disk recovery that has a dangling by-id
symlink, a LUKS1 sibling, and two distinct disks sharing one braid
label can surface any subset of the two warnings -- or none -- before
the collision aborts.

The single CLI consumer at `cli/src/main.rs:813-847` only prints warnings
on the `Ok` arm; the `Err` arm prints the error and exits. So even
warnings that *are* collected never reach the operator on the failure
path.

This is exactly the shape **decision 022** (`docs/decisions/022-dry-run-preview-model.md:54-58`)
prescribes a fix for:

> "When planning accumulates notes and then fails later, use a report
> shape that returns both the error and the accumulated notes. The
> command wrapper renders those notes to stderr before returning the
> error..."

`cli/src/mount.rs:163-201` already implements that shape as `PlanReport
{ events, result: Result<..., MountError> }` with a thin outer wrapper
around an `&mut events`-threading inner function. This plan mirrors that
idiom in `discover.rs`, plus finishes the scan loop past the first
structural error so the accumulated warning set is the complete set the
scan would have produced.

Intended outcome:
- Every warning the scan produces (across all entries) reaches stderr,
  including on the failure path.
- The mixed-hazard regression test is deterministic regardless of host
  `read_dir` order, and detects a regression of the early-return
  behavior (not just a regression of the warning-bundling shape).
- A small testable helper enforces the "warnings before error" stderr
  contract at the CLI boundary, so it can't silently regress.

## Approach

### 1. Sort `read_dir` entries by filename before iterating

The current code at `cli/src/discover.rs:285-297` already collects the
`ReadDir` iterator into a `Vec<std::fs::DirEntry>` and hard-fails per-entry
iterator errors via `collect::<Result<Vec<_>, _>>().map_err(DiscoverError::ReadDir)?`.
This plan must preserve that behavior -- silently dropping per-entry
errors with `.flatten()` would turn an incomplete `/dev/disk/by-id` scan
into a partial successful discover/write, which is worse than the
sibling-warning loss the rest of this plan fixes.

The change here is one inserted line -- sort the collected Vec by
filename before the existing `for entry in entries` loop at line 305:

```rust
let entries = match std::fs::read_dir(by_id_dir) {
    Ok(it) => it,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PoolMembership::empty()),
    Err(e) => return Err(DiscoverError::ReadDir(e)),
};
let mut entries: Vec<std::fs::DirEntry> = entries
    .collect::<Result<Vec<_>, _>>()
    .map_err(DiscoverError::ReadDir)?;
entries.sort_by_key(|e| e.file_name());      // <-- new
for entry in entries {
    // existing per-entry body, unchanged
}
```

(The NotFound arm returns `PoolMembership::empty()` rather than a
`DiscoverOutcome` because section 4 deletes that struct.)

Rationale:
- Preserve current behavior: per-entry `ReadDir` errors still hard-fail
  as `DiscoverError::ReadDir`. Only the per-entry iteration order changes.
- Production: makes the "first recorded `LabelCollision`" deterministic
  (lex-smallest filename of the colliding pair under the alias-dedup
  rules) instead of host-`read_dir`-order-dependent. Cost is negligible
  -- `/dev/disk/by-id/` has tens of entries in practice.
- Tests: the mixed-hazard regression test (section 8a) seeds entries
  whose filenames lex-sort *collision before warning* so the test
  exercises the exact code path the section-2 fix protects -- the
  collision is encountered before the dangling-symlink and LUKS1
  entries, and only the loop-continue guarantee keeps their warnings
  in scope. Without sorting, the test could pass by accident on a host
  whose `read_dir` happened to visit warnings first; with sorting, a
  regression to the old early-return behavior fails the test on every
  host.

### 2. Finish the scan loop past the first `LabelCollision`

Inside the inner scan function, replace the mid-loop
`return Err(label_collision(...))` at `cli/src/discover.rs:451-455`
with a record-and-continue accumulator:

```rust
let mut first_collision: Option<DiscoverError> = None;
// ... existing accumulator variables (members, warnings) ...

for entry in entries {  // the sorted Vec from section 1
    // ... existing skip/warn/continue branches stay verbatim ...

    match members.entry(disk_name) {
        Entry::Vacant(e) => { e.insert(candidate); }
        Entry::Occupied(mut e) => {
            let existing = e.get();
            if existing.canonical != candidate.canonical {
                // Distinct disks under the same braid label. Record the
                // first occurrence, keep the existing entry, drop the
                // colliding candidate, and keep scanning so later entries
                // still get to push warnings.
                if first_collision.is_none() {
                    first_collision = Some(label_collision(
                        e.key().as_str(),
                        existing.by_id.as_str().to_owned(),
                        candidate.by_id.as_str().to_owned(),
                    ));
                }
                continue;
            }
            // Same physical disk via two aliases -- existing tie-break
            let candidate_better = (candidate.priority, candidate.filename.as_str())
                < (existing.priority, existing.filename.as_str());
            if candidate_better {
                e.insert(candidate);
            }
        }
    }
}

if let Some(err) = first_collision {
    return Err(err);
}
// ... existing duplicate-UUID post-loop pass and membership insert ...
```

Notes:
- The existing precedence rule (`LabelCollision` before `DuplicateUuid`,
  pinned by `discover_label_collision_fires_before_duplicate_uuid` at
  `cli/src/discover.rs:1560`) is preserved: the post-loop check returns
  `first_collision` before the UUID-dedup pass runs.
- The colliding candidate is *not* inserted into `members`. The existing
  entry stays as the named member; the `seen_uuids` pass still operates
  on a consistent map. (If the post-loop UUID pass would have fired on
  the dropped candidate, returning the recorded `LabelCollision` first
  preserves the established precedence.)
- Second-or-later collisions for the same name and any collisions for
  other names are silently dropped beyond the first record. The error
  surface stays one-error-at-a-time, matching the current contract.

### 3. Introduce `DiscoverScan` and split into outer + inner

In `cli/src/discover.rs`:

```rust
/// Outcome of a discover scan. `warnings` are always populated, even
/// when `result` is `Err`, so callers can render them to stderr before
/// propagating the structural error -- per
/// `docs/decisions/022-dry-run-preview-model.md`.
pub struct DiscoverScan {
    pub warnings: Vec<DiscoverWarning>,
    pub result: Result<PoolMembership, DiscoverError>,
}

pub fn discover_pool_members<R: CommandRunner>(runner: &R) -> DiscoverScan {
    discover_from_dir(runner, &crate::recover::RealByIdResolver, Path::new("/dev/disk/by-id"))
}

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    resolver: &dyn crate::recover::ByIdResolver,
    by_id_dir: &Path,
) -> DiscoverScan {
    let mut warnings = Vec::new();
    let result = discover_from_dir_inner(runner, resolver, by_id_dir, &mut warnings);
    DiscoverScan { warnings, result }
}

fn discover_from_dir_inner<R: CommandRunner>(
    runner: &R,
    resolver: &dyn crate::recover::ByIdResolver,
    by_id_dir: &Path,
    warnings: &mut Vec<DiscoverWarning>,
) -> Result<PoolMembership, DiscoverError> { /* sections 1+2 body */ }
```

Mirror the doc-comment style on `mount.rs`'s `PlanReport` /
`plan_open_pool` / `plan_open_pool_inner` (`cli/src/mount.rs:163-212`).

### 4. Remove the now-redundant `DiscoverOutcome`

`DiscoverOutcome` (`cli/src/discover.rs:131-137`) exists only to bundle
`PoolMembership` with `Vec<DiscoverWarning>` on the success path. Once
warnings live on `DiscoverScan`, it collapses to just `PoolMembership`.
Delete it and adjust the two consumers:

- `render_preview_lines(outcome: &DiscoverOutcome)` -> `render_preview_lines(members: &PoolMembership)` (`cli/src/discover.rs:141-148`).
- `write_discovered_membership(outcome: DiscoverOutcome, ...)` -> `write_discovered_membership(members: PoolMembership, ...)` (`cli/src/discover.rs:538-566`). The body already pulls `outcome.members` out; the change is one less indirection.

### 5. Add a testable CLI rendering helper

Add to `cli/src/discover.rs`:

```rust
/// Drain `scan.warnings` to `out`, one per line, then yield the
/// structural result. Always emits the warning preamble -- including
/// when `result` is `Err` -- so the CLI cannot silently regress to
/// printing the structural error without its sibling-disk warnings.
/// The unit test at the bottom of this module pins the
/// warnings-before-error stderr contract.
pub fn drain_warnings<W: std::io::Write>(
    scan: DiscoverScan,
    out: &mut W,
) -> Result<PoolMembership, DiscoverError> {
    for w in &scan.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    scan.result
}
```

(Lossy `writeln!` ignores write errors against stderr, matching the
existing `eprintln!` semantics in `main.rs`.)

### 6. Rewire the single CLI call site

`cli/src/main.rs:813-847` becomes:

```rust
let runner = RealRunner;
let scan = braid_cli::discover::discover_pool_members(&runner);
let members = match braid_cli::discover::drain_warnings(scan, &mut std::io::stderr()) {
    Ok(m) => m,
    Err(e) => {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
};
if members.is_empty() {
    eprintln!("no braid-labeled LUKS devices found");
    std::process::exit(1);
}
for line in braid_cli::discover::render_preview_lines(&members) {
    eprintln!("{line}");
}
if args.write {
    match braid_cli::discover::write_discovered_membership(
        members, &paths, args.expect_count,
    ) {
        Ok(_) => eprintln!("pool membership written to {}", pool_json.display()),
        Err(e) => { print_cli_error(&e.to_string()); std::process::exit(1); }
    }
} else {
    eprintln!("pass --write to save to {}", pool_json.display());
}
```

The helper call replaces the inline warning loop. A future refactor that
wants to skip warnings would have to delete the `drain_warnings` call --
much more conspicuous than reordering an inline `for warning in ...`.

### 7. Mechanically update existing test call sites

All in `cli/src/discover.rs`'s `mod tests`. Two pattern groups.

**Group A: 18 `discover_from_dir(...)` call sites.**

- 13 success-path sites currently `discover_from_dir(...).unwrap()` and
  read `outcome.members` / `outcome.warnings`. Replace with:
  ```rust
  let scan = discover_from_dir(...);
  let members = scan.result.unwrap();
  // existing assertions on `members` and `scan.warnings`
  ```
- 5 error-path sites currently take `.unwrap_err()` or its `let err = ...`
  variant (`discover_propagates_runner_error_at_isluks:713`,
  `discover_propagates_runner_error_at_luksdump:769`,
  `discover_fails_on_label_collision_across_disks:1198`,
  `discover_duplicate_uuid_surfaces_friendly_error:1509`,
  `discover_label_collision_fires_before_duplicate_uuid:1560`).
  Replace with:
  ```rust
  let scan = discover_from_dir(...);
  let err = scan.result.unwrap_err();
  // existing match on err
  ```

The 12 success-path tests that already assert on `.warnings` keep their
assertions, with `outcome.warnings` -> `scan.warnings`.

**Group B: 6 `DiscoverOutcome { ... }` literal construction sites.**

These tests construct `DiscoverOutcome` directly to feed
`render_preview_lines` / `write_discovered_membership`. With
`DiscoverOutcome` deleted, drop the wrapper and pass `members` directly.

Replace every:
```rust
let outcome = DiscoverOutcome {
    members,
    warnings: Vec::new(),
};
write_discovered_membership(outcome, &paths, ...)
```
with:
```rust
write_discovered_membership(members, &paths, ...)
```
and likewise `render_preview_lines(&members)` for the preview test.

The six sites (lines from the current tree):

- `cli/src/discover.rs:667` -- `render_preview_lines` test in
  `discover_orders_members_by_disk_name`.
- `cli/src/discover.rs:1596` -- `discover_write_refuses_when_pending_op_exists`.
- `cli/src/discover.rs:1629` -- `discover_write_refuses_when_pool_json_is_name_keyed`.
- `cli/src/discover.rs:1801` -- the happy-write test (no count gate).
- `cli/src/discover.rs:1838` -- `discover_write_refuses_when_count_mismatches_below`.
- `cli/src/discover.rs:1882` -- `discover_write_refuses_when_count_mismatches_above`.

Each is a 3-line drop (remove the `let outcome = DiscoverOutcome { ... };`
binding and pass `members` directly to the consumer).

### 8. Add two new unit tests

#### 8a. `discover_surfaces_warnings_alongside_structural_error`

In `cli/src/discover.rs`'s `mod tests`. Preamble in the literal `//`
line-comment form required by `docs/testing.md:11`:

```rust
// Intent: warnings accumulated during the scan survive a structural
//   error return so the operator sees all sibling hazards in one pass.
// Why it exists: every `return Err(...)` inside discover used to drop
//   the warning vec, and the LabelCollision early-return inside the
//   entry loop additionally skipped warnings that later entries would
//   have produced. Fixing both paths requires a test that pins both
//   guarantees: warnings survive the error return, and warnings from
//   entries scanned after the collision still appear.
// Scenario: multi-disk recovery -- the operator has a dangling by-id
//   symlink, a LUKS1 leftover, and two distinct disks sharing
//   `braid-foo`; `braid discover` must report all three hazards so they
//   can be addressed before retry.
#[test]
fn discover_surfaces_warnings_alongside_structural_error() {
    let dir = tempfile::tempdir().unwrap();
    // Dangling symlink -> CannotCanonicalize warning.
    discover_create_by_id_symlink(dir.path(), "ata-DANGLING", "/nonexistent/dangling");
    // LUKS1 disk -> UnsupportedLuksVersion warning.
    let luks1_target = discover_create_target(dir.path(), "fake-luks1");
    let luks1_alias = discover_create_by_id_symlink(dir.path(), "ata-LUKS1", &luks1_target);
    // Two distinct disks with the same braid label -> LabelCollision.
    let target_a = discover_create_target(dir.path(), "fake-sda");
    let target_b = discover_create_target(dir.path(), "fake-sdb");
    let alias_a = discover_create_by_id_symlink(dir.path(), "ata-CLONE_A", &target_a);
    let alias_b = discover_create_by_id_symlink(dir.path(), "ata-CLONE_B", &target_b);
    let runner = DiscoverLabelMap::new(&[
        (&luks1_alias, "braid-legacy"),
        (&alias_a,     "braid-foo"),
        (&alias_b,     "braid-foo"),
    ])
    .with_version(&luks1_alias, 1);

    let scan = discover_from_dir(&runner, &crate::recover::RealByIdResolver, dir.path());

    assert!(
        matches!(&scan.result, Err(DiscoverError::LabelCollision { .. })),
        "expected LabelCollision, got {:?}", scan.result,
    );
    assert!(
        scan.warnings.iter().any(|w| matches!(
            w, DiscoverWarning::CannotCanonicalize { path, .. } if path.ends_with("ata-DANGLING")
        )),
        "expected CannotCanonicalize warning, got: {:?}", scan.warnings,
    );
    assert!(
        scan.warnings.iter().any(|w| matches!(
            w, DiscoverWarning::UnsupportedLuksVersion { path, version: 1 }
                if path.ends_with("ata-LUKS1")
        )),
        "expected UnsupportedLuksVersion warning, got: {:?}", scan.warnings,
    );
}
```

The fixture API used is `DiscoverLabelMap::with_version(path, 1)`
(`cli/src/test_fixtures/discover.rs:43-46`), not `with_luks_version`.
The test is deterministic on every host because section 1 sorts the
`read_dir` entries by filename and the seeded names lex-sort with the
collision first (`ata-CLONE_A`, `ata-CLONE_B`, `ata-DANGLING`,
`ata-LUKS1`). Combined with section 2's loop-continue rule, this means
the test exercises -- and a regression of either change would fail --
the exact "collision encountered first; later warnings still surface"
code path.

#### 8b. `drain_warnings_writes_warnings_before_returning_error`

In the same `mod tests`. Pins the helper's stderr contract independently
from the inline `discover_from_dir` data flow, so a future refactor that
keeps `scan.warnings` populated but moves the warning loop somewhere
that runs only on `Ok` cannot pass this test.

```rust
// Intent: drain_warnings writes every warning to `out` before
//   returning, even when `scan.result` is `Err`.
// Why it exists: pins the CLI's "warnings before error" stderr ordering
//   at the helper boundary so it cannot silently regress to printing
//   the structural error first and the warnings never (the pre-fix
//   bug shape).
// Scenario: any structural error surfaced after warnings accumulated
//   (label collision, duplicate uuid, ...). The unit test passes a
//   synthetic DiscoverScan rather than driving discover_from_dir so it
//   stays a contract test of the helper, not of the scan.
#[test]
fn drain_warnings_writes_warnings_before_returning_error() {
    let scan = DiscoverScan {
        warnings: vec![
            DiscoverWarning::CannotCanonicalize {
                path: "/dev/disk/by-id/ata-DANGLING".into(),
                detail: "no such file".into(),
            },
            DiscoverWarning::UnsupportedLuksVersion {
                path: "/dev/disk/by-id/ata-LEGACY".into(),
                version: 1,
            },
        ],
        result: Err(DiscoverError::LabelCollision {
            name: "foo".into(),
            path1: "/dev/disk/by-id/ata-A".into(),
            path2: "/dev/disk/by-id/ata-B".into(),
        }),
    };

    let mut buf: Vec<u8> = Vec::new();
    let err = drain_warnings(scan, &mut buf).expect_err("expected Err");
    let out = String::from_utf8(buf).unwrap();

    assert!(out.contains("ata-DANGLING"), "missing dangling warning: {out}");
    assert!(out.contains("ata-LEGACY"), "missing legacy warning: {out}");
    assert!(matches!(err, DiscoverError::LabelCollision { .. }));
}
```

## Files to modify

- `cli/src/discover.rs`
  - Sort `read_dir` entries by filename (section 1).
  - Convert the mid-loop `LabelCollision` return into a
    record-and-continue accumulator (section 2).
  - Add `DiscoverScan` and split `discover_pool_members` /
    `discover_from_dir` into outer + `_inner` (section 3; reshapes
    lines 193-201 and 280-522).
  - Remove `DiscoverOutcome` (lines 131-137) and reshape
    `render_preview_lines` (lines 141-148) /
    `write_discovered_membership` (lines 538-566) signatures (section 4).
  - Add `drain_warnings` (section 5).
  - Update 18 `discover_from_dir(...)` call sites and 6
    `DiscoverOutcome { ... }` literal sites in `mod tests` (section 7).
  - Append the two new tests in section 8.
- `cli/src/main.rs`
  - Reshape the `discover` command arm (lines 813-847) per section 6.

## What we are NOT changing

- `DiscoverError` variants stay as-is
  (`Cmd`/`ReadDir`/`LabelCollision`/`DuplicateUuid`).
- Error precedence (`LabelCollision` > `DuplicateUuid` > insert
  failure) stays as-is.
- One-error-at-a-time semantics stay: only the first
  `LabelCollision` is returned; subsequent collisions are dropped.
- `DiscoverWriteError::Discover(#[from] DiscoverError)` (line 188)
  stays. Currently unreferenced but harmless; deleting it is a separate
  cleanup.
- No new error variants, no eager stderr printing inside the scan, no
  behavior change to `DiscoverWarning` `Display` impls.
- No VM test. Unit-level coverage is sufficient because the entire
  warning-on-error surface is pure data plumbing and one helper.

## Verification

1. `just test-rust` -- all 24 updated test sites (18
   `discover_from_dir` callers + 6 `DiscoverOutcome` literals) pass; the
   two new tests (`discover_surfaces_warnings_alongside_structural_error`,
   `drain_warnings_writes_warnings_before_returning_error`) pass.
2. `cargo build` (covered by `just test-rust`) -- confirms the
   `main.rs` call-site reshuffle and the `DiscoverOutcome`-removal
   cascade compile.
3. Read-through verification of the `main.rs` arm: the only path from
   `discover_pool_members(...)` to a `members` binding goes through
   `drain_warnings`, so every code path that uses members has already
   written warnings to stderr.
4. (Optional) `just test-parsers` -- unaffected by this fix, but
   confirms no incidental regression in the discover path.
