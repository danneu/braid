# Fix `too_many_arguments` in lock.rs close-set helpers

## Context

`cargo clippy` flags `build_close_sets_full` (9 args) with `too_many_arguments`
(9/7). It is not alone: two tightly-coupled helpers in the same file thread the
**identical** group of five `&mut` out-params and exceed the threshold too:

| Function | Loc | Args | Status |
| --- | --- | --- | --- |
| `push_uuid_classified_candidate` | `cli/src/lock.rs:306` | 10 | flagged |
| `build_close_sets_full` | `cli/src/lock.rs:942` | 9 | the pasted warning |
| `build_close_sets_uuid_scanned_fallback` | `cli/src/lock.rs:1062` | 8 | flagged |

The five shared out-params are: `notes`, `skipped_mappers`, `cleanup_uncertain`,
`members_potentially_present`, `has_unclassified_skip`. They are populated during
mapper classification and then drained by `plan_lock` into the returned `LockPlan`.

The fix (per the requested approach): bundle those five into one owned struct,
`CloseSetAccumulator`, threaded through all three helpers as a single
`&mut CloseSetAccumulator`. This clears all three warnings and replaces a
recurring out-param group with a named domain type rather than the codebase's
other option (`#[allow(clippy::too_many_arguments)]`, used in `recover.rs`).

Chosen over a borrowed-ref `CloseSetSink<'a>` (struct of five `&mut` refs)
because: it models a real concept (the classification outcome), needs no
lifetime, introduces no `&mut`-bundle pattern foreign to braid (existing param
structs `AddParams`/`AddStepsInput`/`DoctorContext` hold only immutable refs),
reads cleaner at each site (`acc.cleanup_uncertain = true`, no deref), and can
later host the paired `cleanup_uncertain`+`has_unclassified_skip` invariant as a
method (a follow-up, not this change).

This is a pure mechanical refactor: no control flow, values, or behavior change.

## The new type

Define near the other close-set types (e.g. just above `push_orphan_close` at
`cli/src/lock.rs:280`, or beside `LockCloseSet` ~133). All fields impl `Default`.

```rust
/// Plan-level outputs the three close-set classification helpers accumulate
/// into, so `build_close_sets_full`, `build_close_sets_uuid_scanned_fallback`,
/// and `push_uuid_classified_candidate` share one sink instead of threading
/// five `&mut` out-params. `plan_lock` owns it and drains the fields into the
/// `LockPlan` after classification.
#[derive(Default)]
struct CloseSetAccumulator {
    notes: Vec<PreviewNote>,
    skipped_mappers: Vec<MapperName>,
    cleanup_uncertain: bool,
    members_potentially_present: HashSet<DiskName>,
    has_unclassified_skip: bool,
}
```

No new imports: `PreviewNote`, `MapperName`, `HashSet`, `DiskName` are already in
scope in `lock.rs`.

## Changes

`member_owned` / `orphan_mappers` stay **local** to the two `build_close_sets_*`
functions (they are consumed by `LockCloseSet::from_classified`, not plan-level
outputs) and remain explicit `&mut Vec<LockMapperClose>` params to
`push_uuid_classified_candidate`. `push_orphan_close` (`:280`, 4 args) is
**unchanged** -- callers just pass `&mut acc.notes` instead of `notes`.

### 1. `push_uuid_classified_candidate` (`:306`) -> 6 args

Replace params `notes, skipped_mappers, cleanup_uncertain, members_potentially_present,
has_unclassified_skip` with `acc: &mut CloseSetAccumulator`. Keep `runner, mapper,
membership, member_owned, orphan_mappers`. Body edits:

- `:320` `members_potentially_present.insert(..)` -> `acc.members_potentially_present.insert(..)`
- `:327` `push_orphan_close(notes, ..)` -> `push_orphan_close(&mut acc.notes, ..)`
- `:330` `notes.push(..)` -> `acc.notes.push(..)`
- `:333` `skipped_mappers.push(..)` -> `acc.skipped_mappers.push(..)`
- `:334` `*cleanup_uncertain = true` -> `acc.cleanup_uncertain = true`
- `:335` `*has_unclassified_skip = true` -> `acc.has_unclassified_skip = true`

### 2. `build_close_sets_full` (`:942`) -> 5 args

Keep `runner, fs, pool, membership`; replace the five out-params with
`acc: &mut CloseSetAccumulator`. Body edits:

- `:959`, `:978`, `:999` `members_potentially_present.insert(..)` -> `acc.members_potentially_present.insert(..)`
- `:970`, `:990` `push_orphan_close(notes, &mut orphan_mappers, ..)` -> `push_orphan_close(&mut acc.notes, &mut orphan_mappers, ..)`
- `:994`, `:1029` `notes.push(..)` -> `acc.notes.push(..)`
- `:1002` `skipped_mappers.push(..)` -> `acc.skipped_mappers.push(..)`
- `:1003`, `:1030` `*cleanup_uncertain = true` -> `acc.cleanup_uncertain = true`
- `:1031` `*has_unclassified_skip = true` -> `acc.has_unclassified_skip = true`
- `:1040-1052` `push_uuid_classified_candidate(..)` call: drop the five out-param
  args, pass `acc` last (auto-reborrows in the `for` loop):
  `push_uuid_classified_candidate(runner, mapper, membership, &mut member_owned, &mut orphan_mappers, acc)`

### 3. `build_close_sets_uuid_scanned_fallback` (`:1062`) -> 4 args

Keep `runner, fs, membership`; replace the five out-params with
`acc: &mut CloseSetAccumulator`. Body edits:

- `:1078` `notes.push(..)` -> `acc.notes.push(..)`
- `:1079` `*cleanup_uncertain = true` -> `acc.cleanup_uncertain = true`
- `:1080` `*has_unclassified_skip = true` -> `acc.has_unclassified_skip = true`
- `:1085-1097` `push_uuid_classified_candidate(..)` call: same as above, pass `acc`.

### 4. `plan_lock` call sites + drain (`:845-932`)

- `:845-849`: replace the five `let mut ...` declarations with a single
  `let mut acc = CloseSetAccumulator::default();`. Keep `pause_balance_before_unmount`
  (`:850`) as its own local -- it is **not** part of the accumulator.
- `:865` (Probed arm): `build_close_sets_full(runner, fs, pool, membership, &mut acc)`.
- `:878` (ProbeFailed arm): `notes.push(..)` -> `acc.notes.push(..)`.
- `:891` and `:902` (ProbeFailed / Unmounted arms):
  `build_close_sets_uuid_scanned_fallback(runner, fs, membership, &mut acc)`.
- `:913-917`: `members_known_closed(membership, &acc.members_potentially_present, acc.has_unclassified_skip)`.
- `:919-932` `LockPlan { .. }`: `notes: acc.notes`, `skipped_mappers: acc.skipped_mappers`,
  `cleanup_uncertain: acc.cleanup_uncertain`. Other fields unchanged. Borrow-safe:
  `members_known_closed` borrows/copies before the field moves; `members_potentially_present`
  is never moved out (partial move OK), `has_unclassified_skip` is `Copy`.

### 5. Tests (`mod tests`, `use super::*` at `:1316`)

Delete both now-redundant adapter wrappers -- after the refactor they would be
pure passthroughs (their sole job was injecting the two out-params tests ignored,
now covered by `CloseSetAccumulator::default()`):

- `build_close_sets_full_for_test` (`:3988`)
- `build_close_sets_uuid_scanned_fallback_for_test` (`:4012`)

Update their 9 callsites (`:4558, 4628, 4681, 4724, 4788, 5023, 5145` for the
full variant; `:4915, 4980` for the fallback) with one mechanical pattern each:

- Replace the three locals (`let mut notes`, `let mut skipped`, `let mut cleanup_uncertain`)
  with `let mut acc = CloseSetAccumulator::default();`.
- Call the real helper directly (in scope via `use super::*`):
  `build_close_sets_full(&runner, &fs, &pool, &membership, &mut acc)` /
  `build_close_sets_uuid_scanned_fallback(&runner, &fs, &membership, &mut acc)`.
- Repoint assertions/derivations to the fields: `skipped` -> `acc.skipped_mappers`,
  `cleanup_uncertain` -> `acc.cleanup_uncertain`, `notes` -> `acc.notes`
  (assertion sites incl. `:4703-4704, 4746-4747, 4812-4813, 4822, 4935, 4996, 5035-5036`).
  `plan.cleanup_uncertain` sites (`:5072, 5097`) read `LockPlan` and are untouched.

## Verification

Pure refactor, no behavior change, no parser/tool-version change -> no VM tests
or fixture refresh needed. The classification logic is covered by the lock.rs
unit tests being updated.

1. `just test-rust` -- the lock.rs unit tests assert the same close-set / notes /
   skipped / cleanup_uncertain outcomes; they must pass unchanged in meaning.
2. `cargo clippy --manifest-path cli/Cargo.toml --tests` (i.e. `just clippy`) --
   confirm the three close-set `too_many_arguments` warnings (`lock.rs:306/942/1062`)
   are gone and no new warnings are introduced. Post-refactor arg counts: full=5,
   fallback=4, push_uuid=6 (all under 7). The clippy run will NOT be fully clean:
   three unrelated `too_many_arguments` warnings are pre-existing and out of scope --
   `enroll_key_file.rs:620`, `lock.rs:1224` (`cmd_lock_impl_with_notes`), and
   `recover.rs:3414` -- and must remain untouched. (Measured: 6 such warnings total
   before this change, 3 after.)

## Out of scope

- Folding the paired `cleanup_uncertain` + `has_unclassified_skip` writes (3 sites)
  into a `CloseSetAccumulator::mark_unclassified_skip()` method. The owned struct
  enables this and it would harden the "set both, not one" invariant per AGENTS.md
  Mutation Safety Heuristics, but it is a behavior-adjacent follow-up, not part of
  this lint fix.

## Follow Up

- Add a `CloseSetAccumulator::mark_unclassified_skip()` helper for the paired `cleanup_uncertain` plus `has_unclassified_skip` writes in `cli/src/lock.rs` once that behavior-adjacent invariant hardening is planned.
