# Pin the multi-missing auto-resolve path in `resolve_replace_source`

## Context

`resolve_replace_source` (`cli/src/replace.rs:1648-1763`) resolves which device
`braid replace` targets. On the dead-disk path with no `--missing-id`, it
auto-resolves by returning the devid recorded in `pool.json`
(`old_member.devid`) after confirming that devid appears in the btrfs missing
set (`cli/src/replace.rs:1732-1760`).

This matches the current documented contract. Per ADR 012
(`docs/design/decisions/012-intent-cli.md:66`, status **Active**, as clarified
by commit `be079e1`):

> The missing devid is auto-resolved from `--old`'s persisted pool.json devid,
> cross-checked against `PoolState::missing_devids` -- independent of how many
> devices are missing. Because `--old`'s name already identifies the member, no
> missing-count gate is needed; `--missing-id` is an optional cross-check (it
> must equal the persisted devid, else `OldDevidMismatch`) and is never
> required.

`docs/commands/replace.md` (lines 36, 75, 112) agrees: `--missing-id` is an
optional cross-check, never required. The code already conforms -- there is no
`missing_devids.len()` gate anywhere in the replace path.

The gap is **test coverage**, not behavior. Every existing dead-disk test uses a
single-element or empty `missing_devids` (`vec![]`, `vec![2]`, `vec![3]` at
lines 2187, 2493, 2522, 2627, 2738, ...). Nothing pins the part of the contract
the ADR calls out explicitly: when two or more devids are missing, the
*persisted* devid -- not `missing_devids[0]` -- selects the target, and no flag
is required. A future refactor that indexed `missing_devids[0]`, or that
re-introduced a `missing_devids.len() > 1 -> require --missing-id` guard, would
pass the entire current suite while silently violating the Active ADR. This is
purely additive test coverage; no production code or doc changes.

> Scope note: an earlier iteration of this plan proposed adding a
> `missing_devids.len() > 1` refusal guard. That was based on a prior revision
> of ADR 012 / `replace.md` that required `--missing-id` for multiple missing
> devices. Commit `be079e1` superseded that contract; adding the guard now would
> make the code contradict the Active ADR. The plan is therefore test-only.

Scope confirmed: add the one load-bearing test only. The previously-considered
"Case 2" refusal sibling is omitted -- it exercises the same
`!missing_devids.contains(&persisted_devid)` branch already pinned by
`persisted_devid_not_in_missing_set_rejected` (`cli/src/replace.rs:2733`) and
adds no distinct regression coverage.

## Change

Add one Rust unit test to `mod tests` in `cli/src/replace.rs`, placed adjacent
to the existing single-missing tests (immediately after
`dead_old_resolution_with_devid`, which ends at line 2538), so the
single/multiple pair reads together.

The test reuses the established helpers in this module -- `two_device_pool()`
(line 2169), `disk_name()` (line 2203), `mp()` (line 1978), `MockRunner` -- and
the existing mutate-`two_device_pool` pattern used by every other dead-disk
test. No new fixture or helper is needed.

Per `docs/dev/testing.md:11`, the contiguous `// Intent` / `// Why it exists` /
`// Scenario` preamble block goes **directly above** the test item, with
`#[test]` after it:

```rust
// Intent: with two devids missing and no `--missing-id`, auto-resolve selects
//   the devid recorded in pool.json (the persisted devid), independent of the
//   missing count, per ADR 012.
// Why it exists: the auto-resolve-independent-of-count contract
//   (ADR 012 line 66; `missing_devids.contains(&persisted_devid)` then return
//   the persisted devid) is unverified -- every other dead-disk test uses a
//   single-element missing set. A regression that indexed `missing_devids[0]`,
//   or re-added a `missing_devids.len() > 1 -> require --missing-id` guard,
//   would pass all existing tests while violating the Active ADR.
// Scenario: two devices are missing (devids 2 and 3); the operator runs
//   `braid replace --old disk3` with no `--missing-id`, and pool.json records
//   the old member as devid 3.
#[test]
fn dead_old_resolution_multiple_missing_picks_persisted_devid() {
    let mut pool = two_device_pool();
    // Drop the live disk2 entry and model a second missing device so the btrfs
    // missing set has two devids. Only disk1 (devid 1, UUID 111...) stays live,
    // so the UUID-keyed live find cannot match the old member.
    pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
    pool.missing_count = 2;
    pool.total_devices = 3;
    pool.missing_devids = vec![2, 3];
    let runner = MockRunner::default();
    // Old member records devid 3 -- the SECOND entry in missing_devids, so a
    // `missing_devids[0]` regression would wrongly pick 2. Its UUID is absent
    // from pool.devices so resolution flows to the missing arm, not the live
    // arm.
    let uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
    let member = membership::DiskMember {
        name: disk_name("disk3"),
        by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
        devid: Some(3),
        added_at: None,
    };
    let result = resolve_replace_source(
        &runner,
        &disk_name("disk3"),
        &uuid,
        &member,
        None,
        &pool,
        &mp(),
    );
    assert!(
        matches!(result, Ok(ReplaceSource::Missing { devid: 3 })),
        "expected Missing {{ devid: 3 }} (persisted devid disambiguates the \
         two-element missing set), got: {result:?}"
    );
}
```

### Why this is the ideal shape

- **Right level.** The disambiguation is a pure function of
  `(missing_devids, persisted_devid, missing_id)`. `resolve_replace_source` is
  the natural unit boundary and the level every sibling test already targets --
  no VM test (`replace-dead-disk.py`) or higher-level harness is warranted.
- **Behavioral, structure-insensitive assertion.** It asserts on the return
  contract (`Ok(ReplaceSource::Missing { devid: 3 })`), so it survives any
  refactor that preserves the documented behavior.
- **Catches the feared regressions.** Choosing `devid: 3` (the second element of
  `[2, 3]`) means a `missing_devids[0]` regression returns `2 != 3`, and a
  re-introduced `len() > 1 -> require --missing-id` guard returns `Err` -- both
  fail this test.

## Traced behavior (confirmation, against current code)

Old UUID `333...` is absent from `pool.devices` (only disk1 = `111...` remains),
so the live find at line 1658 does not match. `persisted_devid = 3`;
`missing_id = None` enters the auto-resolve branch (line 1732);
`!missing_devids.contains(&3)` is false, so the refusal block is skipped and the
function returns `persisted_devid = 3` -> `Ok(ReplaceSource::Missing { devid: 3 })`.

## No code or documentation changes

The code already implements the Active ADR's auto-resolve-independent-of-count
contract, and ADR 012 + `replace.md` already describe it. This is a test-only
addition.

(Optional, explicitly out of scope: the inline comment at
`cli/src/replace.rs:1754-1759` still frames the case as "the operator must
supply `--missing-id` ... UNLESS the persisted devid pinpoints exactly one,"
which reads as stale next to the clarified ADR even though its conclusion --
return the persisted devid -- is correct. Modernizing that comment is a nicety,
not part of this test-only change.)

## Existing tests stay green

The new test is purely additive. No existing `resolve_replace_source` test uses
a multi-element `missing_devids`, so none overlaps or conflicts; the
single-missing auto-resolve (`dead_old_resolution_single_missing`) and all
`--missing-id` / live / `devid: None` cases are unaffected.

## Verification

Pure Rust unit test -- no fixture refresh, no VM test, no docs change.

1. Focused run, new test plus the dead-disk siblings:
   `cargo test -p braid-cli dead_old_resolution` (expect all pass, including the
   new one).
2. Full unit suite to confirm nothing else moved: `just test-rust`.

Optional sanity check that the test actually pins the contract: temporarily edit
the auto-resolve branch to return `pool.missing_devids[0]` instead of
`persisted_devid`, confirm the new test fails with `Missing { devid: 2 }`, then
revert.

## Files

- `cli/src/replace.rs` -- add one `#[test]` fn in the existing `mod tests`,
  after `dead_old_resolution_with_devid` (ends line 2538).
- No changes to `docs/commands/replace.md` or
  `docs/design/decisions/012-intent-cli.md` (already match the code).
