# Pivot: dedup the `added_at` invariant, encode the intentional message divergence

## Context

A review finding (Low/Simplicity) flagged three near-identical foreign-device
rejection messages and the `added_at` precedence ladders across
`validate_live_members_allowed`, `build_membership_from_live_pool`, and
`recover_membership_matching_expected` as copy-paste that has "drifted," and
proposed routing all three through one shared rejection constructor plus one
`added_at` helper.

Investigation showed the finding is half right:

- **The `added_at` ladder is a genuine duplicate of a documented invariant.**
  Three identical ladders (`recover.rs:1645-1649`, `1681-1685`, `1895-1899`)
  implement Decision 017's rule verbatim
  (`docs/design/decisions/017-runtime-disk-membership.md:80`): "preserve
  each member's `added_at` from the current `pool.json` if present, else from
  the journal's pre/target membership snapshot; only members with no prior
  timestamp get a fresh `now_iso()` stamp." The Explore sweep confirmed the
  pattern is recover-only -- `replace.rs:855` uses a simpler single-level
  fallback, not this ladder. Centralizing it puts the documented invariant in
  one place.

- **The message divergence is intentional, not drift.**
  `recover_membership_matching_expected` checks against
  `journal.target_membership` in post-commit paths -- "is not part of the
  expected committed membership" is accurate. The other two check against
  `union_memberships()` = `pre_membership` U `target_membership`
  (`recover.rs:3611`) -- "has no by-id path in either the pre-operation or
  target membership snapshot" is literally accurate. Decision 017:80 documents
  exactly this phase-specific split ("post phases mount from the committed
  target membership ... replace pool-mutation recovery uses the pre/target
  union"), and two tests pin the two wordings
  (`recover.rs:11017-11035`, `10991-11007`). Merging all three through one
  constructor would force one message to misstate what it checked.

**Outcome of the pivot:** extract the genuinely-duplicated invariant into one
helper; encode the intentional message split in the code (a named constructor
for the union pair, a comment on the committed-target outlier) so this finding
does not recur; do not merge the three messages and do not unify the two
rebuild functions.

## Changes

All edits are in `cli/src/recover.rs`. No behavior changes.

### 1. Extract `resolve_added_at` (private fn)

Place next to the existing shared helper `resolve_by_id_for_underlying`
(`recover.rs:111`), which both rebuild functions already call -- same locality,
same privacy.

```rust
/// Decision 017 `added_at` precedence for recover's pool.json rebuild:
/// the current pool.json (`prior`) wins, else the journal snapshot member
/// (`fallback`), else a fresh stamp. Centralized so the two rebuild paths
/// and the devid re-insertion loop cannot drift from the documented invariant.
fn resolve_added_at(
    prior: Option<&PoolMembership>,
    fallback: &DiskMember,
    uuid: &LuksUuid,
) -> Option<String> {
    prior
        .and_then(|p| p.by_uuid(uuid))
        .and_then(|m| m.added_at.clone())
        .or_else(|| fallback.added_at.clone())
        .or_else(|| Some(crate::util::now_iso()))
}
```

Replace the three ladders with calls (each already has `prior:
Option<&PoolMembership>` in scope):

- `recover.rs:1645-1649` -> `let added_at = resolve_added_at(prior, expected_member, &dev.luks_uuid);`
- `recover.rs:1681-1685` -> `let added_at = resolve_added_at(prior, expected_member, uuid);`
- `recover.rs:1895-1899` -> `let added_at = resolve_added_at(prior, union_member, &dev.luks_uuid);`

Types confirmed: `PoolMembership::by_uuid(&self, &LuksUuid) -> Option<&DiskMember>`
(`membership.rs:258`), `DiskMember.added_at: Option<String>`
(`membership.rs:234`), `now_iso() -> String` (`util.rs:15`).

### 2. Extract one constructor for the two byte-identical union messages

The messages at `recover.rs:1857-1863` (`validate_live_members_allowed`) and
`1886-1892` (`build_membership_from_live_pool`) are character-for-character
identical. Give them one named constructor whose name encodes which membership
set it describes -- this is what distinguishes them from the committed-target
message and prevents the next reviewer from "fixing" the non-bug.

```rust
/// Foreign-device rejection for the union-rebuild paths
/// (`validate_live_members_allowed`, `build_membership_from_live_pool`): a live
/// device absent from the pre-operation U target snapshot. Worded distinctly
/// from `recover_membership_matching_expected`'s committed-target rejection by
/// design (Decision 017 phase-specific membership) -- do not merge the two.
fn foreign_live_device_not_in_snapshot(dev: &PoolDevice) -> RecoverError {
    RecoverError::Failed(format!(
        "device {} (LUKS UUID {}) is in the live pool but has no by-id path in either \
         the pre-operation or target membership snapshot.\n\
         This must be resolved manually -- provide the correct \
         /dev/disk/by-id/ path and re-run recovery.",
        dev.mapper.0, dev.luks_uuid
    ))
}
```

Route both sites through it. `PoolDevice` (the `pool.devices` element type,
`types.rs:469`) is not imported in production scope: the top-level
`use crate::types::{...}` at `recover.rs:24-26` lists `ByIdPath, ConfigDiskState,
DiskName, LuksUuid, MountPoint, PoolState, format_uuid_list` -- not `PoolDevice`
(only the test module imports it, at `recover.rs:3680`). **Add `PoolDevice` to that
`use crate::types::{...}` list** as part of this change, or the constructor will not
compile. `RecoverError::Failed(String)` is at `recover.rs:48`.

### 3. Annotate the committed-target outlier

Leave the message in `recover_membership_matching_expected`
(`recover.rs:1638-1642`) inline -- it is single-use and deliberately different.
Add a one-line comment above it noting the wording intentionally differs from
the union paths because this path checks the committed target membership, citing
Decision 017's phase-specific membership rule. This is the recurrence-prevention
piece: it answers, in code, the question the finding asked.

## Tests

**No new tests.** The change is a behavior-preserving extraction, and the observable
`added_at` and rejection-wording contracts are already covered end-to-end through
`cmd_recover` / the rebuild functions (run via `just test-rust`). These existing tests
are the regression guard -- they must stay green unchanged, and their doing so is the
proof the extraction preserves behavior:

- `added_at` precedence (Decision 017), full ladder, all driven through `cmd_recover`:
  - `recover_preserves_added_at_from_current_pool_json` (`recover.rs:13552`) -- prior
    pool.json wins over the journal snapshot.
  - `recover_preserves_added_at_from_journal_when_pool_json_absent` (`13606`) -- journal
    fallback when pool.json is absent.
  - `recover_stamps_fresh_added_at_when_no_prior_record` (`13669`) -- fresh `now_iso()`
    stamp when neither source has a timestamp.
  - `recover_membership_matching_expected_reinserts_missing_devid_member` (`11047`) and
    `..._reinserts_null_underlying_member` (`11147`) -- prior-over-fallback precedence on
    the devid-only re-insertion path.
- Rejection wording (the intentional split this plan encodes in code):
  - `recover_membership_matching_expected_rejects_foreign_live_uuid` (`11017`) -- committed
    wording.
  - `build_membership_from_live_pool_rejects_foreign_live_uuid` (`10991`) -- union wording.

A dedicated unit test on `resolve_added_at` is intentionally omitted: calling the private
helper directly would pin its existence and signature, so it would fail under a
behavior-preserving refactor that re-inlined or renamed the helper (structure-sensitive),
and it would be redundant with the end-to-end coverage above. Likewise no test for the
message constructor: it is a verbatim string move already pinned by the two rejection
tests.

## Verification

- `just test-rust` -- the recover unit tests live here; this is the primary and
  sufficient gate. The change is pure Rust with no tool-output parsing, systemd,
  or mount/lock behavior touched.
- No fixture refresh and no NixOS VM tests required (no parser-critical tool,
  module, or lifecycle surface is involved).

## Rejected alternatives

- **One shared constructor for all three messages (the finding's literal fix).**
  Rejected: the committed-target vs pre/target-union wording is intentional and
  Decision-017-backed; a single message would misstate what was checked in one path.
- **Unify `recover_membership_matching_expected` and `build_membership_from_live_pool`.**
  Rejected: they differ in membership source (committed target vs union) and the
  former carries a devid-only re-insertion loop (`recover.rs:1670-1709`) the latter
  lacks; parameterizing both would add more branching than it removes.
- **Put `resolve_added_at` in `membership.rs`.** Rejected: the ladder is
  recover-only (`replace.rs:855` uses a different, simpler fallback), so a shared
  module placement would imply a cross-module contract that does not exist.
