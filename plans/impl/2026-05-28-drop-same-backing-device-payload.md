# Drop the unused `device` payload from `LivePoolMatch::SameBacking`

## Context

`LivePoolMatch::SameBacking` (`cli/src/add.rs:125`) carries a `device: &'a PoolDevice` field that no production code reads. All three production sites destructure with `{ .. }` (lines 320, 2062, 2127) -- the no-op `continue` and stale-plan arms only need to know that the variant fired. The field exists for visual symmetry with `DifferentBacking { device }` and to back one over-specified test assertion at line 2703. Carrying an inert borrow forces every production callsite to write `SameBacking { .. }` and keeps a lifetime tie on the variant for no behavioral reason.

Outcome: a unit `SameBacking` variant, three cleaner production match arms, and a test that asserts the variant rather than introspecting a payload that is unambiguous by construction.

## Scope

Single-file change, all in `cli/src/add.rs`. The enum is not referenced elsewhere in the repo (confirmed by survey of `cli/src/`, `cli/tests/`, `docs/`, `plans/`).

## Changes

1. **Enum definition (`cli/src/add.rs:124-128`).** Convert `SameBacking { device: &'a PoolDevice }` to a unit variant:

   ```rust
   enum LivePoolMatch<'a> {
       SameBacking,
       DifferentBacking { device: &'a PoolDevice },
       NoMatch,
   }
   ```

   Keep the derive list (`Debug, Clone, Copy, PartialEq, Eq`) and the `'a` lifetime -- `DifferentBacking` still needs it. Update the variant-level rustdoc to drop the device-field framing and explain why `SameBacking` is a unit variant (no production reader; same-backing is the no-op signal).

2. **Constructor (`cli/src/add.rs:267`).** Change `Ok(LivePoolMatch::SameBacking { device })` to `Ok(LivePoolMatch::SameBacking)`. The surrounding `same_backing.get_or_insert(device)` accumulator is no longer needed for the construction itself; simplify to a bool (`let mut same_backing = false;` then `same_backing = true;` at the assignment site, and `else if same_backing { Ok(LivePoolMatch::SameBacking) }`). This preserves the existing "DifferentBacking wins over SameBacking" precedence and the "scan all matching rows so a later canonicalize failure surfaces" behavior covered by the test at line 2794.

3. **Production callsites.** Replace `LivePoolMatch::SameBacking { .. }` with `LivePoolMatch::SameBacking` at:
   - `cli/src/add.rs:320` (stale-plan error arm in `validate_braid_preconditions`)
   - `cli/src/add.rs:2062` (`continue` arm in the mapper-open SamePool branch)
   - `cli/src/add.rs:2127` (`continue` arm in the planning-loop SamePool branch)

4. **Test (`cli/src/add.rs:2691-2708`, `live_pool_match_same_backing`).** Replace the `match`-with-payload-introspection with a single equality assertion matching the style of `live_pool_match_no_uuid` at line 2759:

   ```rust
   let result = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap();
   assert_eq!(result, LivePoolMatch::SameBacking);
   ```

   The previous `device.mapper == MapperName("braid-drifted".into())` assertion was redundant: the pool fixture contains exactly one matching device, so a `SameBacking` result already proves which row matched. The test's stated Intent ("recognizes a UUID match as already live only after target by-id and pool row backing path match") is fully proven by the variant.

   Leave the `// Intent / Why it exists / Scenario` preamble unchanged.

## What does not change

- `DifferentBacking`'s payload stays. Production at `cli/src/add.rs:313`, `:2064`, `:2129` feeds `device` into `duplicate_live_pool_uuid_error`, and the test at `:2835` asserts which row was selected when multiple matching rows exist.
- `classify_live_pool_match`'s contract and scanning behavior. Tests at `live_pool_match_different_backing`, `live_pool_match_canonicalize_error`, `live_pool_match_canonicalize_error_after_different_backing`, and `live_pool_match_mixed_same_and_different_backing` continue to pass without edits.
- Public API and the planner / executor flow. No callers outside this file.

## Verification

- `just test-rust` (or `cargo test -p braid-cli classify_live_pool_match` and the surrounding test module). All `live_pool_match_*` tests must pass, in particular:
  - `live_pool_match_same_backing` (rewritten assertion)
  - `live_pool_match_different_backing` (unchanged, sanity that the other arm still works)
  - `live_pool_match_mixed_same_and_different_backing` (proves DifferentBacking precedence is preserved by the constructor simplification)
  - `live_pool_match_canonicalize_error_after_different_backing` (proves we still scan past a SameBacking-eligible row to surface later errors)
- `cargo build -p braid-cli` to confirm no stray lifetime/borrow issues from the constructor simplification.
- No VM tests required; this is an internal type refactor with no user-visible behavior change.
