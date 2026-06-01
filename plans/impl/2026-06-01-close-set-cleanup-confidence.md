# Replace `CloseSetAccumulator`'s paired cleanup booleans with `CleanupConfidence`

## Context

The prior commit (`45905c4`) bundled five lock close-set out-params into one
owned `CloseSetAccumulator` (`cli/src/lock.rs`). Two of those fields are a
coupled pair:

- `cleanup_uncertain: bool` -- cleanup may be incomplete even if no close step
  exists; drives the "cleanup incomplete" preview note and suppresses the "pool
  already locked" line.
- `has_unclassified_skip: bool` -- a skip whose membership cannot be pinned
  down; fully suppresses `members_known_closed` (the planner withholds *every*
  known-closed claim because absence can no longer be proven).

`has_unclassified_skip` always implies `cleanup_uncertain`, but the reverse is
false: a **duplicate-devid** skip leaves cleanup uncertain yet does *not* warrant
suppressing known-closed for *unrelated* members, because the colliding members
are individually added to `members_potentially_present` and so excluded
precisely. The two booleans encode three valid states plus one nonsensical one
(`cleanup_uncertain = false, has_unclassified_skip = true`). Two sites must
remember to set both bits together; the existing `Out of scope` / `Follow Up`
notes in `plans/impl/2026-06-01-fix-close-set-args.md` flagged this as the next
hardening step.

This plan replaces the pair with a single tri-state enum so the invalid
combination is unrepresentable and the classified-vs-unclassified distinction is
named, while keeping the public `LockPlan.cleanup_uncertain: bool` output
unchanged. It is a behavior-preserving refactor (current production already does
*not* over-suppress on duplicate-devid, since that site sets only
`cleanup_uncertain`).

Scope is confined to `cli/src/lock.rs` (`CloseSetAccumulator` is private; the
free `members_known_closed` fn has exactly one caller, both in this file).

## Design

### New enum (place immediately above `CloseSetAccumulator`, ~`lock.rs:166`)

```rust
/// Tri-state cleanup confidence replacing the coupled `cleanup_uncertain` /
/// `has_unclassified_skip` booleans, so the planner cannot represent the
/// nonsensical "suppress known-closed without uncertainty" combination.
/// `IncompleteClassified` (e.g. duplicate-devid) marks cleanup uncertain but
/// keeps unrelated absent members eligible for `members_known_closed`;
/// `IncompleteUnclassified` (classify / `/dev/mapper` scan failure) additionally
/// withholds every known-closed claim because absence can no longer be proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CleanupConfidence {
    #[default]
    Complete,
    IncompleteClassified,
    IncompleteUnclassified,
}

impl CleanupConfidence {
    /// Record incomplete cleanup whose affected members are still individually
    /// accounted for. Never downgrades an existing unclassified state.
    fn mark_incomplete_classified(&mut self) {
        if matches!(self, Self::Complete) {
            *self = Self::IncompleteClassified;
        }
    }

    /// Record an unverifiable skip whose membership cannot be pinned down.
    /// Dominant state (top of the lattice), so assigning it can never downgrade.
    fn mark_incomplete_unclassified(&mut self) {
        *self = Self::IncompleteUnclassified;
    }

    /// Mirror of the old `cleanup_uncertain` boolean for the output-facing
    /// `LockPlan` field, the preview note, and the "pool already locked" gate.
    fn is_uncertain(&self) -> bool {
        !matches!(self, Self::Complete)
    }

    /// Whether the planner must withhold every `members_known_closed` claim.
    fn suppresses_known_closed(&self) -> bool {
        matches!(self, Self::IncompleteUnclassified)
    }
}
```

Escalation is monotonic and order-independent (the one subtle property the enum
introduces that the independent-bit code got for free): `mark_incomplete_classified`
is guarded so it cannot clobber `IncompleteUnclassified`; `mark_incomplete_unclassified`
assigns the top element unconditionally. Whether duplicate-devid (classified) or a
classify/scan failure (unclassified) fires first in a single `plan_lock` run, the
result is `IncompleteUnclassified`.

### `CloseSetAccumulator` (~`lock.rs:168-175`)

Replace the two booleans with one field; keep the other three unchanged. Tidy
the struct doc to say "cleanup confidence as a single tri-state" instead of the
two coupled booleans.

```rust
#[derive(Default)]
struct CloseSetAccumulator {
    notes: Vec<PreviewNote>,
    skipped_mappers: Vec<MapperName>,
    members_potentially_present: HashSet<DiskName>,
    cleanup: CleanupConfidence,
}
```

### `members_known_closed` free fn (~`lock.rs:537-553`)

Drive suppression from the enum, not a free boolean. Change the third param from
`has_unclassified_skip: bool` to `cleanup: CleanupConfidence` (the enum is
`Copy`), and the guard body:

```rust
fn members_known_closed(
    membership: &PoolMembership,
    members_potentially_present: &HashSet<DiskName>,
    cleanup: CleanupConfidence,
) -> Vec<DiskName> {
    if cleanup.suppresses_known_closed() {
        return Vec::new();
    }
    // ... unchanged filter ...
}
```

## Assignment-site mapping

Re-grep before editing (do not trust these line anchors):
`rg -n "cleanup_uncertain = true|has_unclassified_skip = true|acc\.has_unclassified_skip|cleanup_uncertain: acc" cli/src/lock.rs`

| Site (current anchor) | What it is | Old writes | New |
| --- | --- | --- | --- |
| `push_uuid_classified_candidate` `Err` arm (~341-342) | classify failure | both bools | `acc.cleanup.mark_incomplete_unclassified();` |
| Pass 2 duplicate-devid (~987) | classified incomplete | `cleanup_uncertain` only | `acc.cleanup.mark_incomplete_classified();` |
| `build_close_sets_full` scan failure (~1014-1015) | `/dev/mapper` scan failed | both bools | `acc.cleanup.mark_incomplete_unclassified();` |
| `build_close_sets_uuid_scanned_fallback` scan failure (~1055-1056) | `/dev/mapper` scan failed | both bools | `acc.cleanup.mark_incomplete_unclassified();` |

Reads in `plan_lock`:

- `members_known_closed(...)` call (~891-895): pass `acc.cleanup` instead of
  `acc.has_unclassified_skip`. `acc.cleanup` is `Copy`, so it copies before the
  later field moves; `members_potentially_present` is still only borrowed.
- `LockPlan { .. }` drain (~908): `cleanup_uncertain: acc.cleanup.is_uncertain()`.
  `LockPlan.cleanup_uncertain: bool` (def ~573) and its two readers
  (`preview` ~580, `execute` ~778) are **unchanged** -- this is the narrowest
  compatibility-preserving boundary.

## Tests (all in `cli/src/lock.rs`)

### 1. Mechanical: accumulator-field reads (`acc.cleanup_uncertain` -> `acc.cleanup.is_uncertain()`)

Five direct-call sites read the removed field; rewrite each to the method,
preserving exact meaning:

- `assert!(!acc.cleanup.is_uncertain())` at ~4610, ~4644, ~4819 (complete cases)
- `assert!(acc.cleanup.is_uncertain())` at ~4700, ~4905 (incomplete cases)

`plan.cleanup_uncertain` assertions (~2953, 2986, 3114, 4942, 4967) read the
unchanged `LockPlan` field and stay as-is.

### 2. Pin classified-vs-unclassified -- the coverage gap

The unclassified-suppression side is **already pinned** and needs no change:
`full_arm_pass3_classify_failure_suppresses_known_closed_members` (~4925) and
`full_arm_scan_failure_suppresses_known_closed_members` (~4957) both keep an
unrelated absent member (`ccc`) and assert `members_known_closed` is empty, which
only holds if suppression fires.

The classified side is **not** pinned. `full_arm_pass2_duplicate_devid_skips_and_warns_with_cleanup_uncertain`
(~4658) uses a 2-member corruption membership (`aaa`, `bbb`, both devid 7); both
become `members_potentially_present`, so its `plan.members_known_closed.is_empty()`
assertion (~4773) passes *vacuously* -- it would pass whether or not duplicate-devid
suppressed unrelated members. Strengthen this existing test so it pins the real
invariant (claimants excluded **and** unrelated absent member retained):

- Add a third, unrelated, absent member to the corruption membership (devid
  `None`, never observed): `let (ccc_uuid, ccc) = disk_member(702, "ccc", "/dev/disk/by-id/c");`
  and append `(ccc_uuid, ccc)` to the `for_corruption_tests(vec![..])` list.
  (`disk_member` is already imported and yields `devid: None`, so it does not
  collide with devid 7 and is not classified into any close set.)
- The earlier direct `build_close_sets_full` assertions in the same test are
  unaffected (`ccc` is unobserved: skipped=`[braid-dup]`, one warn,
  `acc.cleanup.is_uncertain()` true, no `braid-dup` re-probe).
- Replace the final assertion (~4773-4777) with:
  ```rust
  assert_eq!(
      known_closed_names(&plan),
      vec!["ccc"],
      "dup-devid claimants aaa/bbb excluded as potentially-present, but unrelated absent ccc must stay known-closed: {:?}",
      known_closed_names(&plan)
  );
  ```
- Extend the test's `// Why it exists` / `// Scenario` preamble to state that an
  unrelated absent member must remain known-closed under duplicate-devid (the
  classified-incomplete state does not over-suppress).

### 3. Pin the enum's escalation contract (guards the one new risk)

Add a focused unit test (no mocks) for `CleanupConfidence` monotonic escalation,
since collapsing two independent bits into one field is the only place a future
edit could silently downgrade suppression:

```rust
// Intent: CleanupConfidence escalates monotonically -- an unclassified
//   incomplete state dominates a classified one regardless of order, and a
//   classified mark never downgrades an existing unclassified state.
// Why it exists: the enum collapses two independent booleans into one field,
//   so a careless mark could silently clear known-closed suppression; the old
//   paired-bool code could not downgrade because the bits were independent.
// Scenario: one plan_lock run hits both a duplicate-devid skip (classified) and
//   a stranded classify failure (unclassified) across passes.
#[test]
fn cleanup_confidence_unclassified_dominates_classified() {
    let mut c = CleanupConfidence::default();
    assert!(!c.is_uncertain());
    assert!(!c.suppresses_known_closed());

    c.mark_incomplete_classified();
    assert!(c.is_uncertain());
    assert!(!c.suppresses_known_closed());

    c.mark_incomplete_unclassified();
    assert!(c.suppresses_known_closed());

    // No downgrade: a later classified mark must not clear suppression.
    c.mark_incomplete_classified();
    assert!(c.suppresses_known_closed());
    assert_eq!(c, CleanupConfidence::IncompleteUnclassified);
}
```

(`assert_eq!` on the enum is why the derive includes `Debug, PartialEq, Eq`.)

## Verification

Pure in-crate logic refactor: no parser, tool-version, or fixture change, so no
VM tests or fixture refresh.

1. `just test-rust` -- the lock.rs unit tests, including the strengthened
   duplicate-devid test and the new escalation test, must pass.
2. `cargo clippy --manifest-path cli/Cargo.toml --tests` -- no new warnings. The
   only expected baseline is the three pre-existing, unrelated `too_many_arguments`
   warnings (`enroll_key_file.rs:620`, `lock.rs` `cmd_lock_impl_with_notes`,
   `recover.rs:3414`); they are out of scope and untouched. Do **not** expect any
   `result_large_err` lines -- it is `allow`-ed workspace-wide
   (`Cargo.toml` `[workspace.lints.clippy]`, inherited by the CLI crate via
   `cli/Cargo.toml` `[lints] workspace = true`), so it never fires. All four enum
   methods have non-test callers, so no dead-code warning.

## Out of scope

- `LockPlan.cleanup_uncertain` stays a `bool` (output-facing; narrowest change).
- No changes to CLI output wording, preview notes, or the "pool already locked"
  message.
- No broader lock-planning refactor; only the four mark-sites, the one
  `members_known_closed` call/signature, the drain, and the tests above change.
