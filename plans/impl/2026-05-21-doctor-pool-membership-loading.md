# Plan: unify pool-membership loading in doctor checks

## Context

Three doctor checks (`check_declared_disks`, `check_foreign_luks_uuid`,
`check_smart_selftests`) each call `membership::load_membership` and
handle the result with slightly different control flow and slightly
different wording. The divergence has now produced two real bugs:

- **`check_declared_disks`** emits `Ok("all 0 declared disks present")`
  when `pool.json` parses to an empty membership (on-disk shape
  `{"disks":{}}`, also produced by `save_membership(&PoolMembership::empty(), ...)`).
  Misleading green signal on the surface operators consult to diagnose
  "where did my disks go." (`cli/src/doctor.rs:431-439`, missing an
  `is_empty()` guard at `cli/src/doctor.rs:499-526`.)
- **`check_foreign_luks_uuid`** emits `Fail("N foreign LUKS UUIDs in
  live pool: ...")` against an empty membership if the pool is mounted,
  because `membership::foreign_luks_uuids` classifies every live UUID
  as foreign when membership has nothing to compare to
  (`cli/src/membership.rs:676-685`,
  `cli/src/doctor.rs:710-766`). This is louder than the
  `declared_disks` bug -- spurious `Fail`, not just misleading `Ok`.

Note: `PoolMembership` is `#[serde(deny_unknown_fields)]` with a
required `disks` field (`cli/src/membership.rs:221-225`). Literal
`{}` therefore parses as `Corrupt`, NOT empty. The empty-membership
shape is `{"disks":{}}` or, more idiomatically in tests,
`membership::save_membership(&PoolMembership::empty(), &paths)` (see
`cli/src/membership.rs:251` for the `empty()` constructor and the
existing smart_selftests empty-case test at
`cli/src/doctor.rs:2074-2087` for the test pattern). All
empty-membership coverage in this plan uses that shape; `{}` and
`"not-json"` are reserved for the corrupt-`Warn` path.

`check_smart_selftests` already has the right empty-membership guard
(`cli/src/doctor.rs:905-907`) but uses its own wording style
(`"no pool members declared"`), which conflicts with the dominant
`"skipped (...)"` convention used everywhere else in `doctor.rs`
(13 sites; see `cli/src/doctor.rs:213-1126`). It also collapses all
load errors -- including `Corrupt`/`Conflict` -- into `Skip`, while
the other two checks correctly `Warn` on non-`NotFound` errors.

Outcome: one shared helper that loads membership or returns a typed
`CheckResult` for the three not-loadable cases (file missing, file
unreadable/corrupt, file empty). All three checks consume it. The
class of "did this check handle empty/missing/corrupt the same way as
its peers" finding is dissolved.

## Approach

### 1. Add `load_membership_or_check_result` helper in `cli/src/doctor.rs`

Private helper near the existing membership-consuming checks
(suggested placement: just above `check_declared_disks` at
`cli/src/doctor.rs:499`).

Signature:

```rust
/// Load pool membership for a doctor check, or return the unified
/// not-loadable `CheckResult` for the caller to emit. Centralizes the
/// three skip/warn cases (file missing, file unreadable, file empty)
/// so peer checks tell the operator the same thing.
fn load_membership_or_check_result<R: CommandRunner>(
    ctx: &DoctorContext<'_, R>,
    check_name: &'static str,
) -> Result<PoolMembership, CheckResult> {
    match membership::load_membership(ctx.paths) {
        Ok(m) if m.is_empty() => Err(CheckResult::skip(
            check_name,
            "skipped (no pool members declared)",
        )),
        Ok(m) => Ok(m),
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(CheckResult::skip(
                check_name,
                "skipped (no pool membership file)",
            ))
        }
        Err(e) => Err(CheckResult::warn(
            check_name,
            format!("could not load pool membership: {e}"),
        )),
    }
}
```

Wording rationale:
- `"skipped (no pool membership file)"` is already the
  `declared_disks` / `foreign_luks_uuid` wording (`doctor.rs:505`,
  `doctor.rs:725`). Keep it.
- `"skipped (no pool members declared)"` re-uses the existing
  smart_selftests phrase but wraps it in the dominant `"skipped (...)"`
  style (matches 13 other skips in the same file).
- `"could not load pool membership: {e}"` is already the
  `declared_disks` / `foreign_luks_uuid` warn wording
  (`doctor.rs:510`, `doctor.rs:728`). Replaces smart_selftests'
  `"pool membership not enumerable ({e})"`. Aligning to Warn (not
  Skip) for non-NotFound errors is an intentional behavioral change:
  a corrupt or conflicting `pool.json` is a real operator problem,
  not an absence.

### 2. Refactor the three call sites

`check_declared_disks` (`cli/src/doctor.rs:499-526`):

```rust
let pool_membership = match load_membership_or_check_result(ctx, "declared_disks") {
    Ok(m) => m,
    Err(cr) => return cr,
};
// ... existing classify-and-summarize logic unchanged ...
```

`check_foreign_luks_uuid` (`cli/src/doctor.rs:710-766`): same shape,
threading `NAME` as the check name. Place the helper call where the
existing `load_membership` match is (line 720), after the
`config.is_none()` and `mountpoint not mounted` skips.

`check_smart_selftests` (`cli/src/doctor.rs:893-907`): returns
`Vec<CheckResult>`, so wrap the Err arm:

```rust
let membership = match load_membership_or_check_result(ctx, NAME) {
    Ok(m) => m,
    Err(cr) => return vec![cr],
};
// ... existing per-drive iteration unchanged ...
```

Delete the now-redundant `if membership.is_empty()` block at
`doctor.rs:905-907`.

### 3. Test updates and additions in `cli/src/doctor.rs` test module

Update two existing smart_selftest tests (both have wording
assertions that the helper change invalidates):

- `cli/src/doctor.rs:2053-2068`
  (`check_smart_selftest_membership_load_error_emits_unscoped_skip`)
  -- runs with no `pool.json`, so it hits the NotFound arm. After
  the helper change, the message becomes
  `"skipped (no pool membership file)"`. Update the assertion at
  line 2064 from `r.message.contains("pool membership not
  enumerable")` to `assert_eq!(r.message, "skipped (no pool
  membership file)")`. Keep the unscoped `Skip` shape (`r.subject
  == None`, `r.status == Skip`). The preamble comment can stay as
  is -- it still describes the scenario accurately.
- `cli/src/doctor.rs:2074-2087`
  (`check_smart_selftest_no_members_emits_unscoped_skip`) --
  fixture is already correct (uses
  `save_membership(&PoolMembership::empty(), &paths)`). Update
  the assertion at line 2086 from `"no pool members declared"` to
  `"skipped (no pool members declared)"`.

Touch the existing VM-test helper:

- `tests/cli/braid-doctor.py:21-23` -- the `assert_smart_selftest_shape`
  helper already accepts `"pool membership" in msg or "no pool members"
  in msg`; the new wording `"could not load pool membership"` /
  `"skipped (no pool members declared)"` still satisfies the OR. No
  test change required, but verify during VM run.

Add new tests (alongside the existing membership-skip tests at
`cli/src/doctor.rs:2653-2710`):

1. `declared_disks_skips_when_membership_is_empty` -- seed empty
   membership via `membership::save_membership(&PoolMembership::empty(), &paths)`
   into the isolated paths, run doctor, assert `declared_disks` is
   `Skip` with `"skipped (no pool members declared)"`. Follow the
   fixture-setup pattern in
   `declared_disks_skips_when_no_membership_even_if_config_schema_fails`,
   substituting the empty-membership seed for the missing-file
   case.
2. `foreign_luks_uuid_skips_when_membership_is_empty` -- same
   empty-membership seed. Mock the runner so mountpoint check
   returns "mounted" (otherwise the earlier "pool not mounted"
   skip wins). Assert `foreign_luks_uuid` is `Skip` with the
   shared wording -- the key pin is that the previous `Fail` no
   longer fires.
3. `smart_selftest_warns_on_corrupt_membership` -- **required**
   (not optional). Pins the intentional Skip -> Warn behavioral
   change for the non-NotFound err arm in smart_selftests. Write
   a malformed `pool.json` (e.g. literal `{}`, which fails
   `deny_unknown_fields`, or `"not json"`), assert
   `selftest_results_for` returns a single unscoped row with
   `r.status == Warn`, `r.subject == None`, and
   `r.message.contains("could not load pool membership")`. Without
   this test the behavioral change has no regression guard, and a
   future revert to the old Skip wording would slip through.
4. `declared_disks_warns_on_corrupt_membership` -- **required**.
   Pre-refactor, the Warn-on-corrupt branch at
   `cli/src/doctor.rs:507-512` had no test (grep for `"could not
   load pool membership"` in the test module returns zero hits);
   post-refactor it is mediated through `load_membership_or_check_result`,
   so an unguarded helper regression could silently flip the
   semantics for this check too. Write a malformed `pool.json`
   into isolated paths, run doctor, assert `declared_disks` is
   `Warn` with `check.message.contains("could not load pool
   membership")`. Reuse the fixture pattern from
   `declared_disks_skips_when_no_membership_even_if_config_schema_fails`
   (`cli/src/doctor.rs:2694-2710`).
5. `foreign_luks_uuid_warns_on_corrupt_membership` -- **required**.
   Same gap as the declared_disks corrupt-membership case, plus
   the additional pin that the helper must run after the
   `mountpoint not mounted` skip so the Warn fires only when the
   pool is actually mounted. Seed a malformed `pool.json`, mock
   `mountpoint_ok()` and `DoctorMockFs::mounted_btrfs_only()` so
   the earlier skips do not win (reuse the pattern from
   `check_foreign_luks_uuid_skips_when_membership_missing` at
   `cli/src/doctor.rs:3791-3803`), run doctor, assert
   `foreign_luks_uuid` is `Warn` with
   `check.message.contains("could not load pool membership")`.

Skip the pure-summarizer route: `summarize_declared_disks` should
stay rendering-only; the empty-membership decision is a check-level
concern, not a rendering concern, and putting it in the summarizer
would force the renderer to know its own check name.

### 4. Update user-facing manual

`manual/commands/doctor.md:68` -- the `smart_self_test` row claims
"if pool membership cannot be enumerated, a single `Skip` result
with `name: \"smart_self_test\"` is emitted." After the refactor
the `Skip` only holds for missing or empty membership; corrupt or
unreadable membership becomes `Warn`. Reword to distinguish the
two cases. Suggested wording (preserves the scripting guidance):

> ... if pool membership is missing or empty, a single `Skip`
> result with `name: "smart_self_test"` is emitted; if pool
> membership is corrupt or unreadable, a single `Warn` result
> with the same `name` is emitted instead. In both fallbacks
> the `subject` field is omitted. Scripts should check whether
> `subject` is present before keying on it.

`declared_disks` (line 64) and `foreign_luks_uuid` (currently
absent from the table) do not spell out their membership-load
semantics at this level of detail, so they need no manual change
in this plan -- the smart_selftest row is the only stale claim.

## Critical files

- `cli/src/doctor.rs` -- helper + three call-site refactors + tests.
  All Rust edits in one file.
- `manual/commands/doctor.md` -- one-row reword in the "What it
  checks" table.
- `cli/src/membership.rs` -- read-only reference for the
  `PoolMembership::is_empty` (line 330) and `MembershipError` shape
  (line 28). No edits.
- `tests/cli/braid-doctor.py` -- read-only check that the VM
  assertion at line 21-23 still matches the new wording. No edits
  expected.

## Verification

1. `just test-rust` -- new tests pass; the updated smart_selftests
   message assertion passes; no regressions in the existing
   `summarize_declared_disks` pure tests
   (`cli/src/doctor.rs:2720-3022`).
2. `just test-vm braid-doctor` -- VM doctor test still passes; in
   particular the `assert_smart_selftest_shape` helper still matches.
3. Manual sanity for empty membership: in a scratch VM, seed
   empty membership via `echo '{"disks":{}}' > /var/lib/braid/pool.json`
   (NOT `{}`, which is corrupt), run `braid doctor --json`,
   confirm:
   - `declared_disks` row: `Skip`, message
     `"skipped (no pool members declared)"`.
   - `foreign_luks_uuid` row (with pool mounted): `Skip`, same
     message.
   - `smart_self_test` row: single unscoped `Skip` row, same message.
4. Manual sanity for corrupt pool.json: write `pool.json = "{}"`
   or `pool.json = "not json"` (both are corrupt), run `braid
   doctor --json`, confirm all three checks emit `Warn` with
   `"could not load pool membership: ..."`.
5. Spot-check that the reworded `manual/commands/doctor.md`
   `smart_self_test` row matches the actual fallback emitted by
   `braid doctor --json` for both the empty-membership and
   corrupt-membership cases above.

## Out of scope

- Reworking `summarize_declared_disks` -- stays pure-rendering.
- Changing `braid remove` / hand-edit semantics around the empty
  `pool.json` shape. Doctor only reports on it; the question of
  whether `{"disks":{}}` should ever exist on disk is a separate
  decision.
- Other doctor checks that don't load membership.
