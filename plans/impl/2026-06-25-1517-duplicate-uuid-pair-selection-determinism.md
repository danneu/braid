# Plan: pin duplicate-UUID pair-selection determinism for 3+ shared-UUID disks

## Context

`build_membership` in `cli/src/discover.rs` runs a `seen_uuids` pass
(`cli/src/discover.rs:520-538`) that turns the cloned-disk hazard -- two or
more deduped members carrying the same LUKS UUID -- into a friendly
`DiscoverError::DuplicateUuid` that names both disks. The pass iterates
`members: BTreeMap<DiskName, AliasCandidate>` (`cli/src/discover.rs:326`) in
`DiskName` order and **early-returns on the first collision**, then orders the
reported pair's `name1/name2` by lexicographic by-id path.

The error string is operator-facing contract: it must be byte-stable across
rescans and reboots. Two existing tests defend that determinism --
`discover_duplicate_uuid_surfaces_friendly_error` (`:1841`) and
`label_collision_sorts_paths_lexicographically` (`:1580`) -- but **both are
two-disk**. With only two disks there is exactly one possible pair, so those
tests pin the *within-pair* path tie-break and nothing about *which pair* is
selected among N. The selection of which pair to report is therefore unpinned:
it rests on `members` being a name-ordered `BTreeMap` plus the early return. A
refactor (e.g. a `HashMap` accumulator, or sorting all colliding members by
path) could silently change which disks are named while every current test
still passes -- regressing exactly the guarantee those two tests exist to
protect.

This change adds the missing 3-disk regression test and makes the name-order
selection legible in the code so it is not refactored away by accident. It is a
Low-severity testing/robustness fix; no production behavior changes.

## Behavior being pinned

With three braid-labeled disks (distinct names, distinct by-id paths, one
shared UUID), iteration runs in `DiskName` order and returns on the first
collision, so:

- The **reported pair is the two name-smallest members**; the name-largest is
  dropped and only resurfaces on the next scan after the operator detaches one.
- Within that pair, `name1/path1` is the lexicographically-smaller by-id path.

The whole pass is fully deterministic today (all `DiskName`s distinct -> total
`BTreeMap` order; all by-id paths distinct -> total path order). The work is to
keep it that way.

## Test design (the load-bearing part)

A naive 3-disk test where name order and path order *agree* would still pass
under the very regression we want to catch (a `HashMap` accumulator or a
path-order selection would pick the same pair). The test must make name order
and path order **disagree** so the dropped disk's absence is the discriminator.

Give the name-largest disk the lexicographically-smallest by-id path:

| by-id symlink | LUKS label  | DiskName | name order      | path order       |
| ------------- | ----------- | -------- | --------------- | ---------------- |
| `ata-zzz`     | braid-disk1 | disk1    | 1st (smallest)  | 3rd (largest)    |
| `ata-mmm`     | braid-disk2 | disk2    | 2nd             | 2nd              |
| `ata-aaa`     | braid-disk3 | disk3    | 3rd (largest)   | 1st (smallest)   |

All three get distinct canonical targets (distinct physical disks) and the
**same** shared UUID via chained `.with_uuid()`.

Trace: iterate disk1(zzz), disk2(mmm), disk3(aaa). disk1 registers the UUID;
disk2 collides with disk1; within the pair `ata-mmm < ata-zzz`, so the error is
`name1=disk2 / path1=.../ata-mmm`, `name2=disk1 / path2=.../ata-zzz`. disk3
(`ata-aaa`) is never reached.

`disk3` is name-largest (excluded by name-order selection) yet path-smallest
(it would be picked *first* by any path-order or hash-order scheme). So
asserting `disk3`'s absence is what distinguishes correct name-order selection
from a regression.

## Change 1 -- add the regression test

In the `cli/src/discover.rs` test module, immediately after
`discover_label_collision_fires_before_duplicate_uuid` (ends ~`:1920`), add a
test. Build the scenario inline (no shared helper exists, and two tests
pattern-matching one error is not worth abstracting) reusing the existing
fixtures: `DiscoverLabelMap::new(...).with_uuid(...).with_uuid(...).with_uuid(...)`,
`discover_create_target`, `discover_create_by_id_symlink`, `discover_from_dir`
(all in `cli/src/test_fixtures/discover.rs`), mirroring
`discover_duplicate_uuid_surfaces_friendly_error`.

Name (matching the existing convention):
`discover_duplicate_uuid_three_disks_report_name_order_pair`.

Preamble: a contiguous block of `//` line comments directly above `#[test]`
(the literal form in `docs/dev/testing.md`, section "Preamble: literal `//`
line-comment form"), with the exact field labels `Intent:` / `Why it exists:` /
`Scenario:`, **seed 804** (existing seeds are 801-803, 806). Do **not** copy the
neighbor `discover_duplicate_uuid_surfaces_friendly_error`, which uses `///`
doc-comments -- that is a pre-existing deviation from the documented `//`
convention, not the pattern to follow.
- `Intent:` with 3+ disks sharing one LUKS UUID, `DuplicateUuid` reports the two
  name-smallest members in path-sorted order; the name-largest disk is excluded.
- `Why it exists:` two-disk tests pin only the within-pair tie-break, leaving
  pair *selection* (name-ordered `BTreeMap` iteration + first-collision early
  return) unguarded against a refactor to a `HashMap`/path-order accumulator.
- `Scenario:` an original plus two dd-clones (or a clone left mid-swap) all
  present at once; the friendly error must name a stable pair every scan.

Assertions (behavioral, structure-insensitive -- assert on the error contract,
not internals):

1. `scan.result` is `Err(DiscoverError::DuplicateUuid { uuid, name1, path1, name2, path2 })`.
2. `uuid.as_str() == shared_uuid`.
3. `name1.as_str() == "disk2"` and `name2.as_str() == "disk1"` -- pins the
   selected pair *and* the within-pair path order in one shot.
4. `path1.ends_with("ata-mmm")` and `path2.ends_with("ata-zzz")`.
5. **Discriminator:** neither `path1` nor `path2` ends with `ata-aaa`, and the
   rendered `err.to_string()` does **not** contain `braid-disk3`.

Do not re-assert the full remediation wording (`detach the cloned or unintended
disk`, `dd-cloned disk`, both labels) -- that is already covered by the two-disk
`discover_duplicate_uuid_surfaces_friendly_error`; duplicating it here adds
maintenance cost without new coverage.

## Change 2 -- document the selection as intentional (small, separable)

The comment at `cli/src/discover.rs:525-526` explains only the within-pair path
sort. The first-collision-in-name-order *selection* is implicit and
load-bearing. Add a short comment at the top of the `seen_uuids` loop
(~`:521`) noting that `members` iterates in `DiskName` order and the pass
returns on the first collision, so with 3+ shared-UUID disks the reported pair
is the two name-smallest -- the rest surface on the next scan -- and that this
name-order selection is deterministic and pinned by the new test. Keeps a future
refactorer from swapping in a `HashMap` without realizing output stability
depends on the ordered iteration. This is the only production-code touch; it is
a comment, changes no behavior, and can be dropped without affecting the test.

## Non-goals

- **No error-shape change.** The current pair-at-a-time behavior (report two,
  resurface the rest on the next scan) is preserved. Reporting *all* N members
  sharing a UUID at once would be a friendlier multi-clone UX but requires
  changing the two-slot `DuplicateUuid` variant and re-reviewing the
  operator-facing message -- a separate design decision, not this test-gap fix.
- **No shared assertion helper.** Two inline scenarios are clearer than an
  abstraction; the project favors self-contained tests with their own
  Intent/Why/Scenario story.

## Files modified

- `cli/src/discover.rs` -- add one `#[test]` fn (Change 1); add one clarifying
  comment at the `seen_uuids` loop (Change 2).

No other files. This is **not** a parser-compatibility / fixture-refresh event:
the test uses `DiscoverLabelMap`'s synthetic luksDump bodies, invokes no real
tool, and touches no `flake.lock` / pinned-package node. `just test-parsers` and
`just capture-all-fixtures` are not required.

## Verification

1. Quick iteration -- run both duplicate-UUID tests by name substring:
   `cargo test --lib --manifest-path cli/Cargo.toml discover_duplicate_uuid`
   (expect the existing `..._surfaces_friendly_error` and the new
   `..._three_disks_report_name_order_pair` to pass).
2. Confirm the test actually discriminates -- temporarily change the pass to
   iterate `members` in a path-sorted order (or report disk3) and re-run; the
   new test must fail on assertion 3/5 while the two-disk test still passes.
   Revert.
3. Full Rust lane: `just test-rust`.
4. ASCII gate (sanity, though test bodies are exempt):
   `python3 scripts/docs/check-output-ascii.py` -- should stay green.
