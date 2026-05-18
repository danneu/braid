# Plan: tighten `braid replace` UUID-collision refusal bullet

## Context

`manual/commands/replace.md:107` currently reads:

```
- Refuses if the new disk's LUKS UUID is already a member of the pool
```

The planner's pre-journal-write uniqueness check
(`assert_new_uuid_unique` at `cli/src/replace.rs:1417-1436`) actually
refuses on **two** distinct scopes:

1. **Membership** -- the new disk's UUID is already keyed in `pool.json`
   (excluding `old_uuid`, which is being replaced).
2. **LivePool** -- the new disk's UUID matches a UUID observed in the
   live btrfs `pool.devices` set at planning time.

Both raise `ReplaceError::DuplicateUuid { uuid, scope }` rendered through
a single template (`cli/src/replace.rs:60`):

```
duplicate LUKS UUID {uuid} for new replace target: already present in
{scope} -- detach the conflicting disk before retrying
```

The current manual bullet has two problems against this code:

- "already a member of the pool" is operationally close but technically
  imprecise for the LivePool-scope case (a UUID on a live btrfs device
  without a `pool.json` row is not strictly a "member").
- The manual omits the remediation phrase that the error text uses
  ("detach the conflicting disk before retrying"), while the sibling
  bullet at `manual/commands/discover.md:78` already follows the
  "Refuses ... -- detach ..." pattern for the same family of UUID
  collisions.

The proposed pivot is a single-line docs edit -- not the two-bullet
expansion the original finding proposed. Two bullets would surface the
internal `scope` labels (`membership`, `live_pool`) to operators, which
is plumbing they should not have to care about, and would bloat the
cookbook-style refusal section. A one-line tweak that captures both
scopes implicitly with operator-facing language plus the remediation
suffix is sufficient.

The pivot does **not** touch the sibling guard `check_new_not_in_pool`
at `cli/src/replace.rs:1592-1604`. That guard refuses on a different
axis (mapper-name collision in live `pool.devices`, not LUKS UUID); its
existing prose ("is already a member of the pool") is accurate for what
it checks. Re-prose-ing it is unrelated to the documented refusal case
and is left out of scope.

## Change

**File:** `manual/commands/replace.md`
**Line:** 107 (inside the "Safety checks / refusal cases" section)

Replace:

```
- Refuses if the new disk's LUKS UUID is already a member of the pool
```

With:

```
- Refuses if the new disk's LUKS UUID is already in use by the pool (registered membership or live btrfs devices) -- detach the conflicting disk before retrying
```

Rationale for this exact wording:

- "in use by the pool" covers both Membership-scope and LivePool-scope
  collisions without exposing internal-axis jargon.
- The parenthetical "(registered membership or live btrfs devices)"
  gives an operator who hits the `already present in live_pool` error
  enough of a bridge to recognize the manual bullet as the same case.
- The `-- detach the conflicting disk before retrying` suffix is
  verbatim from the error template at `cli/src/replace.rs:60`, so the
  manual and the runtime message agree on the remediation.
- The form mirrors `manual/commands/discover.md:78` ("Refuses the scan
  if two distinct devices share the same LUKS UUID -- detach the cloned
  or unintended disk before retrying.") for stylistic consistency.

No other files change.

## Files referenced (read-only)

- `cli/src/replace.rs:30-65` -- `DuplicateUuidScope` enum and
  `ReplaceError::DuplicateUuid` template (the source of the remediation
  wording).
- `cli/src/replace.rs:1417-1436` -- `assert_new_uuid_unique`, the
  two-scope check the bullet describes.
- `cli/src/replace.rs:5612-5715` -- unit tests that pin both scopes.
- `manual/commands/discover.md:78` -- precedent bullet using the same
  remediation suffix pattern.
- `docs/decisions/024-luks-uuid-identity.md:61-63` -- the "Earlier clone
  and swap detection" decision the bullet supports.

## Verification

1. **Read the diff.** The change is one line. Visually confirm the
   bullet sits cleanly inside the "Safety checks / refusal cases" list
   and the surrounding bullets are untouched.

2. **Render the manual locally (optional).** `just docs` (runs
   `mdbook serve`) builds `manual/book/` and serves it. Open
   `commands/replace.html` and confirm the bullet renders inside the
   refusal list. `manual/book/` is gitignored, so no artifact churn.

3. **Doc-graph sanity.** `just check-docs` verifies `SUMMARY.md` and
   the file tree stay in sync. The edit does not add or remove files,
   so this should pass unchanged.

4. **Test suite is unaffected, but confirm.** The cited integration
   test `tests/cli/replace-new-in-pool-guard.py:69` asserts the
   substrings `"duplicate LUKS UUID"` and `"already present in
   membership"` against the runtime error -- not against the manual
   text -- so the docs edit cannot affect it. Run
   `just test-rust` and `just test-vm replace-new-in-pool-guard` to
   confirm.

5. **Commit message.** Conventional Commits, lowercase first line, e.g.
   `docs(manual): clarify replace UUID-collision refusal bullet`.
